// lib.rs — Knowdly Book Smart Contract
// Deployed on Stellar using the Soroban smart contract platform
//
// This contract handles:
//   1. Book registration by creators
//   2. Book purchases by readers (minting ownership tokens)
//   3. Royalty enforcement on every resale
//   4. Ownership verification for content access control
//   5. Per-wallet token index — get_tokens_by_owner() eliminates localStorage dependency
//   6. update_arweave_tx() — writes real Arweave TX ID after upload completes
//   7. WASM upgrade — preserves all state while updating contract logic
//   8. Marketplace — list_for_sale(), buy_listing(), cancel_listing()
//      Buyer calls buy_listing() which atomically:
//        - verifies the listing exists and price matches
//        - transfers ownership to the buyer
//        - removes the listing
//        - emits a sale event
//      Payment (USDC split) is handled off-chain before calling buy_listing()

#![no_std]

use soroban_sdk::{
    contract,
    contractimpl,
    contracttype,
    symbol_short,
    token,
    Address,
    Env,
    String,
    Vec,
};

// ── Data Types ────────────────────────────────────────────────────────────────

// Book represents a work registered by a creator
#[contracttype]
#[derive(Clone)]
pub struct Book {
    pub id:            u64,
    pub publisher:     Address,
    pub price:         i128,
    pub royalty_bps:   u32,
    pub arweave_tx_id: String,
    pub title:         String,
    pub active:        bool,
    pub total_sales:   u64,
}

// Token represents a reader's ownership of a specific book
#[contracttype]
#[derive(Clone)]
pub struct Token {
    pub id:             u64,
    pub book_id:        u64,
    pub owner:          Address,
    pub minted_at:      u32,
    pub purchase_price: i128,
}

// Listing represents a token listed for resale on the marketplace
#[contracttype]
#[derive(Clone)]
pub struct Listing {
    pub token_id:      u64,
    pub seller:        Address,
    pub asking_price:  i128,
}

// ── Storage Keys ──────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    NextBookId,
    NextTokenId,
    Book(u64),
    Token(u64),
    Ownership(Address, u64),
    Platform,
    PlatformFeeBps,
    PaymentToken,
    OwnerTokens(Address),
    // marketplace listing — keyed by token_id
    Listing(u64),
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct KnowdlyBookContract;

#[contractimpl]
impl KnowdlyBookContract {

    // ── Upgrade ───────────────────────────────────────────────────────────────
    //
    // upgrade() allows the contract WASM to be updated while preserving all
    // existing state (books, tokens, ownership records, listings).
    //
    // Only the platform wallet can call this.
    // new_wasm_hash is obtained by uploading the new WASM to the network first.
    //
    // Usage:
    //   1. Upload new WASM: stellar contract upload --wasm target/.../knowdly_book.wasm
    //   2. Call upgrade(platform, new_wasm_hash) on the existing contract
    //   3. Contract now runs new logic with all existing state intact

    pub fn upgrade(env: Env, platform: Address, new_wasm_hash: soroban_sdk::BytesN<32>) {
        platform.require_auth();

        // only the platform wallet can upgrade
        let stored_platform: Address = env
            .storage().instance()
            .get(&DataKey::Platform)
            .expect("Contract not initialised");

        if stored_platform != platform {
            panic!("Only the platform can upgrade the contract");
        }

        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }

    // set_payment_token — configures which token contract payments settle in
    // (USDC's Stellar Asset Contract address). Separate from initialise() so
    // it can be set on an already-initialised contract via upgrade() without
    // resetting NextBookId/NextTokenId or any other existing state.
    pub fn set_payment_token(env: Env, platform: Address, token: Address) {
        platform.require_auth();

        let stored_platform: Address = env
            .storage().instance()
            .get(&DataKey::Platform)
            .expect("Contract not initialised");

        if stored_platform != platform {
            panic!("Only the platform can set the payment token");
        }

        env.storage().instance().set(&DataKey::PaymentToken, &token);
    }

    // ── Initialisation ────────────────────────────────────────────────────────

    pub fn initialise(env: Env, platform: Address, fee_bps: u32) {
        platform.require_auth();

        if fee_bps > 1000 {
            panic!("Platform fee cannot exceed 10%");
        }

        env.storage().instance().set(&DataKey::Platform,       &platform);
        env.storage().instance().set(&DataKey::PlatformFeeBps, &fee_bps);
        env.storage().instance().set(&DataKey::NextBookId,     &0u64);
        env.storage().instance().set(&DataKey::NextTokenId,    &0u64);
    }

    // ── Creator API ───────────────────────────────────────────────────────────

    pub fn register_book(
        env:           Env,
        publisher:     Address,
        price:         i128,
        royalty_bps:   u32,
        arweave_tx_id: String,
        title:         String,
    ) -> u64 {
        publisher.require_auth();

        if price <= 0         { panic!("Price must be positive"); }
        if royalty_bps > 5000 { panic!("Royalty cannot exceed 50%"); }

        let book_id: u64 = env
            .storage().instance()
            .get(&DataKey::NextBookId)
            .unwrap_or(0);

        let book = Book {
            id:            book_id,
            publisher:     publisher.clone(),
            price,
            royalty_bps,
            arweave_tx_id,
            title,
            active:        true,
            total_sales:   0,
        };

        env.storage().persistent().set(&DataKey::Book(book_id), &book);
        env.storage().instance().set(&DataKey::NextBookId, &(book_id + 1));

        env.events().publish(
            (symbol_short!("reg_book"),),
            (book_id, publisher),
        );

        book_id
    }

    pub fn update_arweave_tx(
        env:           Env,
        publisher:     Address,
        book_id:       u64,
        arweave_tx_id: String,
    ) {
        publisher.require_auth();

        let mut book: Book = env
            .storage().persistent()
            .get(&DataKey::Book(book_id))
            .expect("Book not found");

        if book.publisher != publisher {
            panic!("Only the creator can update this book");
        }

        book.arweave_tx_id = arweave_tx_id;

        env.storage().persistent()
            .set(&DataKey::Book(book_id), &book);

        env.events().publish(
            (symbol_short!("upd_tx"),),
            (book_id,),
        );
    }

    pub fn deactivate_book(env: Env, publisher: Address, book_id: u64) {
        publisher.require_auth();

        let mut book: Book = env
            .storage().persistent()
            .get(&DataKey::Book(book_id))
            .expect("Book not found");

        if book.publisher != publisher {
            panic!("Only the creator can deactivate this book");
        }

        book.active = false;
        env.storage().persistent().set(&DataKey::Book(book_id), &book);
    }

    // ── Reader Purchase API ───────────────────────────────────────────────────

    pub fn purchase(env: Env, buyer: Address, book_id: u64) -> u64 {
        buyer.require_auth();

        let mut book: Book = env
            .storage().persistent()
            .get(&DataKey::Book(book_id))
            .expect("Book not found");

        if !book.active {
            panic!("This book is not available for purchase");
        }

        let already_owned: bool = env
            .storage().persistent()
            .get(&DataKey::Ownership(buyer.clone(), book_id))
            .unwrap_or(false);

        if already_owned {
            panic!("You already own this book");
        }

        let token_id: u64 = env
            .storage().instance()
            .get(&DataKey::NextTokenId)
            .unwrap_or(0);

        let token = Token {
            id:             token_id,
            book_id,
            owner:          buyer.clone(),
            minted_at:      env.ledger().sequence(),
            purchase_price: book.price,
        };

        env.storage().persistent().set(&DataKey::Token(token_id), &token);

        env.storage().persistent().set(
            &DataKey::Ownership(buyer.clone(), book_id),
            &true,
        );

        let owner_key = DataKey::OwnerTokens(buyer.clone());
        let mut owner_tokens: Vec<u64> = env
            .storage().persistent()
            .get(&owner_key)
            .unwrap_or_else(|| Vec::new(&env));
        owner_tokens.push_back(token_id);
        env.storage().persistent().set(&owner_key, &owner_tokens);

        env.storage().instance().set(&DataKey::NextTokenId, &(token_id + 1));

        book.total_sales += 1;
        env.storage().persistent().set(&DataKey::Book(book_id), &book);

        env.events().publish(
            (symbol_short!("purchase"),),
            (token_id, book_id, buyer),
        );

        token_id
    }

    // ── Resale / Transfer API ─────────────────────────────────────────────────
    // transfer_token was removed. It allowed direct ownership transfer with
    // only the seller's signature and a self-reported sale_price that was
    // never actually verified or moved, making it a real bypass of the
    // enforcement in buy_listing. All resales now go through
    // list_for_sale + buy_listing, where payment is enforced on-chain.

    // ── Marketplace API ───────────────────────────────────────────────────────
    //
    // The marketplace allows readers to resell their digital books.
    //
    // Flow:
    //   1. Seller calls list_for_sale(token_id, asking_price)
    //      → Listing stored on-chain, seller retains ownership until sold
    //   2. Buyer pays USDC off-chain (split: seller + creator royalty + platform fee)
    //   3. Buyer calls buy_listing(token_id, buyer_address)
    //      → Verifies listing exists
    //      → Transfers ownership from seller to buyer
    //      → Removes listing
    //      → Emits sale event
    //
    // The buyer calls buy_listing — the seller's auth is NOT required here.
    // This is safe because:
    //   - The seller explicitly listed the token (require_auth in list_for_sale)
    //   - The listing is on-chain — anyone can verify it before paying
    //   - Payment happens before buy_listing is called
    //   - The listing can only be fulfilled once (removed on purchase)

    // list_for_sale — seller lists their token for resale
    // seller must sign this transaction
    pub fn list_for_sale(
        env:          Env,
        seller:       Address,
        token_id:     u64,
        asking_price: i128,
    ) {
        seller.require_auth();

        if asking_price <= 0 {
            panic!("Asking price must be positive");
        }

        // verify seller owns this token
        let token: Token = env
            .storage().persistent()
            .get(&DataKey::Token(token_id))
            .expect("Token not found");

        if token.owner != seller {
            panic!("You do not own this token");
        }

        let listing = Listing {
            token_id,
            seller: seller.clone(),
            asking_price,
        };

        env.storage().persistent().set(&DataKey::Listing(token_id), &listing);

        env.events().publish(
            (symbol_short!("listed"),),
            (token_id, seller, asking_price),
        );
    }

    // cancel_listing — seller removes their listing
    // seller must sign this transaction
    pub fn cancel_listing(env: Env, seller: Address, token_id: u64) {
        seller.require_auth();

        let listing: Listing = env
            .storage().persistent()
            .get(&DataKey::Listing(token_id))
            .expect("Listing not found");

        if listing.seller != seller {
            panic!("You did not create this listing");
        }

        env.storage().persistent().remove(&DataKey::Listing(token_id));

        env.events().publish(
            (symbol_short!("unlisted"),),
            (token_id, seller),
        );
    }

    // buy_listing — buyer completes a resale purchase
    // buyer must sign this transaction
    // payment is verified and moved on-chain, atomically, as part of this
    // same call — if the transfer fails, ownership does not move
    pub fn buy_listing(env: Env, buyer: Address, token_id: u64) {
        buyer.require_auth();

        // verify listing exists
        let listing: Listing = env
            .storage().persistent()
            .get(&DataKey::Listing(token_id))
            .expect("Listing not found");

        // prevent buying your own listing
        if listing.seller == buyer {
            panic!("You cannot buy your own listing");
        }

        // get the token
        let mut token: Token = env
            .storage().persistent()
            .get(&DataKey::Token(token_id))
            .expect("Token not found");

        // verify token is still owned by the seller
        if token.owner != listing.seller {
            panic!("Token owner has changed — listing is invalid");
        }

        let book: Book = env
            .storage().persistent()
            .get(&DataKey::Book(token.book_id))
            .expect("Book not found");

        // ── Payment: computed and enforced atomically, on-chain ─────────────
        let platform_fee_bps: u32 = env
            .storage().instance()
            .get(&DataKey::PlatformFeeBps)
            .unwrap_or(250);

        let platform: Address = env
            .storage().instance()
            .get(&DataKey::Platform)
            .expect("Contract not initialised");

        let payment_token: Address = env
            .storage().instance()
            .get(&DataKey::PaymentToken)
            .expect("Payment token not configured — call set_payment_token first");

        let sale_price      = listing.asking_price;
        let royalty_amount  = (sale_price * book.royalty_bps as i128) / 10_000;
        let platform_amount = (sale_price * platform_fee_bps as i128) / 10_000;
        let seller_amount   = sale_price - royalty_amount - platform_amount;

        if seller_amount < 0 {
            panic!("Asking price too low to cover royalty and platform fees");
        }

        let token_client = token::TokenClient::new(&env, &payment_token);

        // Three transfers, all in this same function call. Soroban requires
        // the buyer's authorisation for each — established once by
        // buyer.require_auth() above and carried through by the client when
        // it simulates and signs this transaction. If any transfer fails —
        // insufficient balance, no trustline, anything — the whole
        // transaction reverts and nothing below this point executes.
        // Ownership only moves if payment actually happened.
        if seller_amount > 0 {
            token_client.transfer(&buyer, &listing.seller, &seller_amount);
        }
        if royalty_amount > 0 {
            token_client.transfer(&buyer, &book.publisher, &royalty_amount);
        }
        if platform_amount > 0 {
            token_client.transfer(&buyer, &platform, &platform_amount);
        }

        // ── Ownership transfer ───────────────────────────────────────────────
        let old_owner = token.owner.clone();
        let book_id   = token.book_id;

        env.storage().persistent().set(
            &DataKey::Ownership(old_owner.clone(), book_id),
            &false,
        );
        env.storage().persistent().set(
            &DataKey::Ownership(buyer.clone(), book_id),
            &true,
        );

        // remove token from seller's list
        let old_key = DataKey::OwnerTokens(old_owner.clone());
        let old_tokens: Vec<u64> = env
            .storage().persistent()
            .get(&old_key)
            .unwrap_or_else(|| Vec::new(&env));
        let mut updated_old = Vec::new(&env);
        for i in 0..old_tokens.len() {
            if old_tokens.get(i).unwrap() != token_id {
                updated_old.push_back(old_tokens.get(i).unwrap());
            }
        }
        env.storage().persistent().set(&old_key, &updated_old);

        // add token to buyer's list
        let new_key = DataKey::OwnerTokens(buyer.clone());
        let mut new_tokens: Vec<u64> = env
            .storage().persistent()
            .get(&new_key)
            .unwrap_or_else(|| Vec::new(&env));
        new_tokens.push_back(token_id);
        env.storage().persistent().set(&new_key, &new_tokens);

        // update token ownership
        token.owner          = buyer.clone();
        token.purchase_price = listing.asking_price;
        env.storage().persistent().set(&DataKey::Token(token_id), &token);

        // remove the listing — can only be fulfilled once
        env.storage().persistent().remove(&DataKey::Listing(token_id));

        env.events().publish(
            (symbol_short!("sold"),),
            (token_id, old_owner, buyer, listing.asking_price),
        );
    }

    // get_listing — returns a listing for a given token
    pub fn get_listing(env: Env, token_id: u64) -> Listing {
        env.storage()
            .persistent()
            .get(&DataKey::Listing(token_id))
            .expect("Listing not found")
    }

    // ── Access Control API ────────────────────────────────────────────────────

    pub fn owns_book(env: Env, owner: Address, book_id: u64) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::Ownership(owner, book_id))
            .unwrap_or(false)
    }

    pub fn get_tokens_by_owner(env: Env, owner: Address) -> Vec<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::OwnerTokens(owner))
            .unwrap_or_else(|| Vec::new(&env))
    }

    // ── Read API ──────────────────────────────────────────────────────────────

    pub fn get_book(env: Env, book_id: u64) -> Book {
        env.storage()
            .persistent()
            .get(&DataKey::Book(book_id))
            .expect("Book not found")
    }

    pub fn get_token(env: Env, token_id: u64) -> Token {
        env.storage()
            .persistent()
            .get(&DataKey::Token(token_id))
            .expect("Token not found")
    }

    pub fn get_total_books(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::NextBookId)
            .unwrap_or(0)
    }

    pub fn get_total_tokens(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::NextTokenId)
            .unwrap_or(0)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    fn test_register_book() {
        let env         = Env::default();
        let contract_id = env.register(KnowdlyBookContract, ());
        let client      = KnowdlyBookContractClient::new(&env, &contract_id);
        let platform    = Address::generate(&env);
        let creator     = Address::generate(&env);

        env.mock_all_auths();
        client.initialise(&platform, &250u32);

        let book_id = client.register_book(
            &creator,
            &10_000_000i128,
            &500u32,
            &String::from_str(&env, "pending_test"),
            &String::from_str(&env, "Introduction to Blockchain"),
        );

        assert_eq!(book_id, 0);
        let book = client.get_book(&book_id);
        assert_eq!(book.id,          0);
        assert_eq!(book.price,       10_000_000);
        assert_eq!(book.royalty_bps, 500);
        assert_eq!(book.active,      true);
        assert_eq!(book.total_sales, 0);
    }

    #[test]
    fn test_update_arweave_tx() {
        let env         = Env::default();
        let contract_id = env.register(KnowdlyBookContract, ());
        let client      = KnowdlyBookContractClient::new(&env, &contract_id);
        let platform    = Address::generate(&env);
        let creator     = Address::generate(&env);

        env.mock_all_auths();
        client.initialise(&platform, &250u32);

        let book_id = client.register_book(
            &creator,
            &10_000_000i128,
            &500u32,
            &String::from_str(&env, "pending_1234567890"),
            &String::from_str(&env, "Test Book"),
        );

        let book = client.get_book(&book_id);
        assert_eq!(book.arweave_tx_id, String::from_str(&env, "pending_1234567890"));

        client.update_arweave_tx(
            &creator,
            &book_id,
            &String::from_str(&env, "real-arweave-tx-id-abc123"),
        );

        let updated = client.get_book(&book_id);
        assert_eq!(updated.arweave_tx_id, String::from_str(&env, "real-arweave-tx-id-abc123"));
    }

    #[test]
    fn test_purchase_and_ownership() {
        let env         = Env::default();
        let contract_id = env.register(KnowdlyBookContract, ());
        let client      = KnowdlyBookContractClient::new(&env, &contract_id);
        let platform    = Address::generate(&env);
        let creator     = Address::generate(&env);
        let reader      = Address::generate(&env);

        env.mock_all_auths();
        client.initialise(&platform, &250u32);

        let book_id = client.register_book(
            &creator,
            &10_000_000i128,
            &500u32,
            &String::from_str(&env, "arweave-tx-id-456"),
            &String::from_str(&env, "Calculus for Engineers"),
        );

        assert_eq!(client.owns_book(&reader, &book_id), false);
        let token_id = client.purchase(&reader, &book_id);
        assert_eq!(client.owns_book(&reader, &book_id), true);

        let token = client.get_token(&token_id);
        assert_eq!(token.book_id, book_id);
        assert_eq!(token.owner,   reader);
    }

    #[test]
    fn test_transfer_enforces_royalty() {
        let env         = Env::default();
        let contract_id = env.register(KnowdlyBookContract, ());
        let client      = KnowdlyBookContractClient::new(&env, &contract_id);
        let platform    = Address::generate(&env);
        let creator     = Address::generate(&env);
        let reader_a    = Address::generate(&env);
        let reader_b    = Address::generate(&env);
        let token_admin = Address::generate(&env);

        env.mock_all_auths();
        client.initialise(&platform, &250u32);

        // set up a mock payment token and fund the buyer
        let token_address      = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
        let token_admin_client = token::StellarAssetClient::new(&env, &token_address);
        let token_client       = token::TokenClient::new(&env, &token_address);
        client.set_payment_token(&platform, &token_address);
        token_admin_client.mint(&reader_b, &8_000_000i128);

        let book_id  = client.register_book(
            &creator,
            &10_000_000i128,
            &500u32,
            &String::from_str(&env, "arweave-tx-id-789"),
            &String::from_str(&env, "Organic Chemistry"),
        );
        let token_id = client.purchase(&reader_a, &book_id);

        client.list_for_sale(&reader_a, &token_id, &8_000_000i128);
        client.buy_listing(&reader_b, &token_id);

        assert_eq!(client.owns_book(&reader_a, &book_id), false);
        assert_eq!(client.owns_book(&reader_b, &book_id), true);

        // verify the royalty and platform fee actually moved, not just ownership
        assert_eq!(token_client.balance(&creator),  400_000);   // 5% of 8_000_000
        assert_eq!(token_client.balance(&platform), 200_000);   // 2.5% of 8_000_000
        assert_eq!(token_client.balance(&reader_a), 7_400_000); // remainder to seller
    }

    #[test]
    fn test_get_tokens_by_owner() {
        let env         = Env::default();
        let contract_id = env.register(KnowdlyBookContract, ());
        let client      = KnowdlyBookContractClient::new(&env, &contract_id);
        let platform    = Address::generate(&env);
        let creator     = Address::generate(&env);
        let reader      = Address::generate(&env);

        env.mock_all_auths();
        client.initialise(&platform, &250u32);

        let book_id_a = client.register_book(
            &creator,
            &10_000_000i128,
            &500u32,
            &String::from_str(&env, "arweave-tx-a"),
            &String::from_str(&env, "Book A"),
        );
        let book_id_b = client.register_book(
            &creator,
            &20_000_000i128,
            &500u32,
            &String::from_str(&env, "arweave-tx-b"),
            &String::from_str(&env, "Book B"),
        );

        let tokens_before = client.get_tokens_by_owner(&reader);
        assert_eq!(tokens_before.len(), 0);

        let token_a = client.purchase(&reader, &book_id_a);
        let token_b = client.purchase(&reader, &book_id_b);

        let tokens_after = client.get_tokens_by_owner(&reader);
        assert_eq!(tokens_after.len(), 2);
        assert_eq!(tokens_after.get(0).unwrap(), token_a);
        assert_eq!(tokens_after.get(1).unwrap(), token_b);
    }

    #[test]
    fn test_tokens_update_on_transfer() {
        let env         = Env::default();
        let contract_id = env.register(KnowdlyBookContract, ());
        let client      = KnowdlyBookContractClient::new(&env, &contract_id);
        let platform    = Address::generate(&env);
        let creator     = Address::generate(&env);
        let reader_a    = Address::generate(&env);
        let reader_b    = Address::generate(&env);
        let token_admin = Address::generate(&env);

        env.mock_all_auths();
        client.initialise(&platform, &250u32);

        let token_address      = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
        let token_admin_client = token::StellarAssetClient::new(&env, &token_address);
        client.set_payment_token(&platform, &token_address);
        token_admin_client.mint(&reader_b, &8_000_000i128);

        let book_id  = client.register_book(
            &creator,
            &10_000_000i128,
            &500u32,
            &String::from_str(&env, "arweave-tx-transfer"),
            &String::from_str(&env, "Transfer Test Book"),
        );
        let token_id = client.purchase(&reader_a, &book_id);

        assert_eq!(client.get_tokens_by_owner(&reader_a).len(), 1);
        assert_eq!(client.get_tokens_by_owner(&reader_b).len(), 0);

        client.list_for_sale(&reader_a, &token_id, &8_000_000i128);
        client.buy_listing(&reader_b, &token_id);

        assert_eq!(client.get_tokens_by_owner(&reader_a).len(), 0);
        assert_eq!(client.get_tokens_by_owner(&reader_b).len(), 1);
        assert_eq!(client.get_tokens_by_owner(&reader_b).get(0).unwrap(), token_id);
    }

    #[test]
    fn test_marketplace_list_and_buy() {
        let env         = Env::default();
        let contract_id = env.register(KnowdlyBookContract, ());
        let client      = KnowdlyBookContractClient::new(&env, &contract_id);
        let platform    = Address::generate(&env);
        let creator     = Address::generate(&env);
        let seller      = Address::generate(&env);
        let buyer       = Address::generate(&env);
        let token_admin = Address::generate(&env);

        env.mock_all_auths();
        client.initialise(&platform, &250u32);

        let token_address      = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
        let token_admin_client = token::StellarAssetClient::new(&env, &token_address);
        let token_client       = token::TokenClient::new(&env, &token_address);
        client.set_payment_token(&platform, &token_address);
        token_admin_client.mint(&buyer, &8_000_000i128);

        let book_id = client.register_book(
            &creator,
            &10_000_000i128,
            &500u32,
            &String::from_str(&env, "arweave-tx-marketplace"),
            &String::from_str(&env, "Marketplace Test Book"),
        );

        // seller purchases the book
        let token_id = client.purchase(&seller, &book_id);
        assert_eq!(client.owns_book(&seller, &book_id), true);

        // seller lists for resale
        client.list_for_sale(&seller, &token_id, &8_000_000i128);

        // verify listing exists
        let listing = client.get_listing(&token_id);
        assert_eq!(listing.token_id,     token_id);
        assert_eq!(listing.seller,       seller.clone());
        assert_eq!(listing.asking_price, 8_000_000);

        // buyer purchases the listing — payment is enforced on-chain,
        // atomically, as part of this same call
        client.buy_listing(&buyer, &token_id);

        // verify ownership transferred
        assert_eq!(client.owns_book(&seller, &book_id), false);
        assert_eq!(client.owns_book(&buyer,  &book_id), true);

        // verify token lists updated
        assert_eq!(client.get_tokens_by_owner(&seller).len(), 0);
        assert_eq!(client.get_tokens_by_owner(&buyer).len(),  1);

        // verify the payment split actually happened
        assert_eq!(token_client.balance(&creator),  400_000);   // 5% royalty
        assert_eq!(token_client.balance(&platform), 200_000);   // 2.5% platform fee
        assert_eq!(token_client.balance(&seller),   7_400_000); // remainder

        // verify listing is removed
        // (would panic if we called get_listing now — listing no longer exists)
    }

    #[test]
    fn test_marketplace_cancel_listing() {
        let env         = Env::default();
        let contract_id = env.register(KnowdlyBookContract, ());
        let client      = KnowdlyBookContractClient::new(&env, &contract_id);
        let platform    = Address::generate(&env);
        let creator     = Address::generate(&env);
        let seller      = Address::generate(&env);

        env.mock_all_auths();
        client.initialise(&platform, &250u32);

        let book_id = client.register_book(
            &creator,
            &10_000_000i128,
            &500u32,
            &String::from_str(&env, "arweave-tx-cancel"),
            &String::from_str(&env, "Cancel Test Book"),
        );

        let token_id = client.purchase(&seller, &book_id);
        client.list_for_sale(&seller, &token_id, &8_000_000i128);

        // cancel the listing
        client.cancel_listing(&seller, &token_id);

        // seller still owns the book
        assert_eq!(client.owns_book(&seller, &book_id), true);
        assert_eq!(client.get_tokens_by_owner(&seller).len(), 1);
    }
}