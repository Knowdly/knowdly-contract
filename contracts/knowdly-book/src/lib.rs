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
//        - verifies the listing exists and the buyer does not already own the book
//        - moves the asking price from the buyer, splitting it between the
//          creator's royalty, the platform fee and the seller
//        - transfers ownership to the buyer
//        - removes the listing
//        - emits a sale event
//      Payment settles inside the invocation; see set_payment_token()

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
    // asset every sale settles in
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

    // ── Initialisation ────────────────────────────────────────────────────────

    pub fn initialise(env: Env, platform: Address, fee_bps: u32) {
        platform.require_auth();

        // without this guard anyone can re-initialise the live contract,
        // overwrite the platform address, and then call upgrade()
        if env.storage().instance().has(&DataKey::Platform) {
            panic!("Contract already initialised");
        }

        if fee_bps > 1000 {
            panic!("Platform fee cannot exceed 10%");
        }

        env.storage().instance().set(&DataKey::Platform,       &platform);
        env.storage().instance().set(&DataKey::PlatformFeeBps, &fee_bps);
        env.storage().instance().set(&DataKey::NextBookId,     &0u64);
        env.storage().instance().set(&DataKey::NextTokenId,    &0u64);
    }

    // set_payment_token — configures the asset every sale settles in
    //
    // Kept separate from initialise() so a contract that is already live can
    // upgrade() into this version and then be configured; the re-init guard
    // above means initialise() is no longer available for that.
    pub fn set_payment_token(env: Env, platform: Address, payment_token: Address) {
        platform.require_auth();

        let stored_platform: Address = env
            .storage().instance()
            .get(&DataKey::Platform)
            .expect("Contract not initialised");

        if stored_platform != platform {
            panic!("Only the platform can set the payment token");
        }

        env.storage().instance().set(&DataKey::PaymentToken, &payment_token);
    }

    pub fn get_payment_token(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::PaymentToken)
            .expect("Payment token not configured")
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

        // move the money before minting anything; a primary sale carries no
        // royalty because the creator is already the seller
        settle_sale(&env, &buyer, &book.publisher, &book.publisher, book.price, 0);

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

    // transfer_token — direct transfer requiring seller signature
    // kept for backwards compatibility
    // for marketplace resales use list_for_sale + buy_listing instead
    pub fn transfer_token(
        env:        Env,
        token_id:   u64,
        new_owner:  Address,
        sale_price: i128,
    ) {
        let mut token: Token = env
            .storage().persistent()
            .get(&DataKey::Token(token_id))
            .expect("Token not found");

        token.owner.require_auth();

        let book: Book = env
            .storage().persistent()
            .get(&DataKey::Book(token.book_id))
            .expect("Book not found");

        // one address holding two copies of a book would corrupt the
        // Ownership flag below, which is a single bool per (address, book)
        let recipient_owns: bool = env
            .storage().persistent()
            .get(&DataKey::Ownership(new_owner.clone(), token.book_id))
            .unwrap_or(false);

        if recipient_owns {
            panic!("Recipient already owns this book");
        }

        // a priced transfer settles here; sale_price 0 is a gift and moves
        // nothing, which also means it pays no royalty
        //
        // the recipient is the payer, so their auth has to be taken in this
        // root invocation: the token contract's own require_auth would be a
        // non-root authorization and the host rejects it
        if sale_price > 0 {
            new_owner.require_auth();
        }

        let (royalty_amount, platform_amount, seller_amount) = if sale_price > 0 {
            settle_sale(
                &env,
                &new_owner,
                &token.owner,
                &book.publisher,
                sale_price,
                book.royalty_bps,
            )
        } else {
            (0i128, 0i128, 0i128)
        };

        let old_owner = token.owner.clone();

        env.storage().persistent().set(
            &DataKey::Ownership(old_owner.clone(), token.book_id),
            &false,
        );
        env.storage().persistent().set(
            &DataKey::Ownership(new_owner.clone(), token.book_id),
            &true,
        );

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

        let new_key = DataKey::OwnerTokens(new_owner.clone());
        let mut new_tokens: Vec<u64> = env
            .storage().persistent()
            .get(&new_key)
            .unwrap_or_else(|| Vec::new(&env));
        new_tokens.push_back(token_id);
        env.storage().persistent().set(&new_key, &new_tokens);

        token.owner          = new_owner.clone();
        token.purchase_price = sale_price;
        env.storage().persistent().set(&DataKey::Token(token_id), &token);

        env.events().publish(
            (symbol_short!("transfer"),),
            (token_id, old_owner, new_owner, royalty_amount, platform_amount, seller_amount),
        );
    }

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
    // payment must be sent off-chain BEFORE calling this
    // the contract transfers ownership and removes the listing atomically
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

        let buyer_owns: bool = env
            .storage().persistent()
            .get(&DataKey::Ownership(buyer.clone(), token.book_id))
            .unwrap_or(false);

        if buyer_owns {
            panic!("You already own this book");
        }

        let book: Book = env
            .storage().persistent()
            .get(&DataKey::Book(token.book_id))
            .expect("Book not found");

        // the buyer's signature on these transfers is what proves payment;
        // without them the asking price is a number in a listing and nothing
        // stops any observer calling buy_listing on a live listing for free
        let (_royalty, _fee, _seller) = settle_sale(
            &env,
            &buyer,
            &listing.seller,
            &book.publisher,
            listing.asking_price,
            book.royalty_bps,
        );

        let old_owner = token.owner.clone();
        let book_id   = token.book_id;

        // transfer ownership to buyer
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

// ── Settlement ────────────────────────────────────────────────────────────────
//
// Splitting a price and moving it are two separate acts on Soroban: the split
// is i128 arithmetic, the movement is a call into the token contract. Deleting
// the calls below leaves code that compiles, succeeds, and emits an event
// describing a payment that never happened, so every sale routes through here.
//
// The payer's signature is re-entered by the token contract for each exact
// amount, which is what makes the buyer's require_auth mean "paid" rather than
// just "asked".

fn settle_sale(
    env:         &Env,
    payer:       &Address,
    seller:      &Address,
    publisher:   &Address,
    total:       i128,
    royalty_bps: u32,
) -> (i128, i128, i128) {
    if total <= 0 {
        panic!("Sale price must be positive");
    }

    let platform: Address = env
        .storage().instance()
        .get(&DataKey::Platform)
        .expect("Contract not initialised");

    let platform_fee_bps: u32 = env
        .storage().instance()
        .get(&DataKey::PlatformFeeBps)
        .unwrap_or(250);

    let royalty_amount  = basis_points(total, royalty_bps);
    let platform_amount = basis_points(total, platform_fee_bps);
    let seller_amount   = total - royalty_amount - platform_amount;

    if seller_amount < 0 {
        panic!("Sale price too low to cover royalty and platform fees");
    }

    let payment_token: Address = env
        .storage().instance()
        .get(&DataKey::PaymentToken)
        .expect("Payment token not configured");

    let asset = token::TokenClient::new(env, &payment_token);

    if royalty_amount > 0 {
        asset.transfer(payer, publisher, &royalty_amount);
    }
    if platform_amount > 0 {
        asset.transfer(payer, &platform, &platform_amount);
    }
    if seller_amount > 0 {
        asset.transfer(payer, seller, &seller_amount);
    }

    (royalty_amount, platform_amount, seller_amount)
}

fn basis_points(amount: i128, bps: u32) -> i128 {
    amount
        .checked_mul(bps as i128)
        .and_then(|scaled| scaled.checked_div(10_000))
        .expect("Fee calculation overflowed")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::token::{StellarAssetClient, TokenClient};
    use soroban_sdk::{testutils::Address as _, Env};

    // deploys a stub asset and points the contract at it, so that the sale
    // paths under test actually have somewhere to move money
    fn payment_asset(
        env:      &Env,
        client:   &KnowdlyBookContractClient,
        platform: &Address,
    ) -> Address {
        let issuer = Address::generate(env);
        let asset  = env.register_stellar_asset_contract_v2(issuer).address();
        client.set_payment_token(platform, &asset);
        asset
    }

    fn fund(env: &Env, asset: &Address, who: &Address, amount: i128) {
        StellarAssetClient::new(env, asset).mint(who, &amount);
    }

    fn balance(env: &Env, asset: &Address, who: &Address) -> i128 {
        TokenClient::new(env, asset).balance(who)
    }

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
        let asset = payment_asset(&env, &client, &platform);
        fund(&env, &asset, &reader, 100_000_000);

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

        env.mock_all_auths();
        client.initialise(&platform, &250u32);
        let asset = payment_asset(&env, &client, &platform);
        fund(&env, &asset, &reader_a, 100_000_000);
        fund(&env, &asset, &reader_b, 100_000_000);

        let book_id  = client.register_book(
            &creator,
            &10_000_000i128,
            &500u32,
            &String::from_str(&env, "arweave-tx-id-789"),
            &String::from_str(&env, "Organic Chemistry"),
        );
        let token_id = client.purchase(&reader_a, &book_id);

        client.transfer_token(&token_id, &reader_b, &8_000_000i128);

        assert_eq!(client.owns_book(&reader_a, &book_id), false);
        assert_eq!(client.owns_book(&reader_b, &book_id), true);
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
        let asset = payment_asset(&env, &client, &platform);
        fund(&env, &asset, &reader, 100_000_000);

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

        env.mock_all_auths();
        client.initialise(&platform, &250u32);
        let asset = payment_asset(&env, &client, &platform);
        fund(&env, &asset, &reader_a, 100_000_000);
        fund(&env, &asset, &reader_b, 100_000_000);

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

        client.transfer_token(&token_id, &reader_b, &8_000_000i128);

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

        env.mock_all_auths();
        client.initialise(&platform, &250u32);
        let asset = payment_asset(&env, &client, &platform);
        fund(&env, &asset, &seller, 100_000_000);
        fund(&env, &asset, &buyer, 100_000_000);

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

        // buyer purchases the listing (payment handled off-chain)
        client.buy_listing(&buyer, &token_id);

        // verify ownership transferred
        assert_eq!(client.owns_book(&seller, &book_id), false);
        assert_eq!(client.owns_book(&buyer,  &book_id), true);

        // verify token lists updated
        assert_eq!(client.get_tokens_by_owner(&seller).len(), 0);
        assert_eq!(client.get_tokens_by_owner(&buyer).len(),  1);

        // verify listing is removed
        // (would panic if we called get_listing now — listing no longer exists)
    }

    // ── Settlement tests ──────────────────────────────────────────────────
    //
    // These assert on balances rather than on the call returning. A test that
    // only checks ownership flipped passes over a payment path that does not
    // exist, which is how the off-chain-payment version of this contract kept
    // a green suite.

    #[test]
    fn test_purchase_pays_the_creator() {
        let env         = Env::default();
        let contract_id = env.register(KnowdlyBookContract, ());
        let client      = KnowdlyBookContractClient::new(&env, &contract_id);
        let platform    = Address::generate(&env);
        let creator     = Address::generate(&env);
        let reader      = Address::generate(&env);

        env.mock_all_auths();
        client.initialise(&platform, &250u32);
        let asset = payment_asset(&env, &client, &platform);
        fund(&env, &asset, &reader, 100_000_000);

        let book_id = client.register_book(
            &creator,
            &10_000_000i128,
            &500u32,
            &String::from_str(&env, "arweave-tx-primary"),
            &String::from_str(&env, "Primary Sale Book"),
        );

        client.purchase(&reader, &book_id);

        // 250 bps of 10_000_000 to the platform, the rest to the creator
        assert_eq!(balance(&env, &asset, &reader),   90_000_000);
        assert_eq!(balance(&env, &asset, &platform),    250_000);
        assert_eq!(balance(&env, &asset, &creator),   9_750_000);
    }

    #[test]
    fn test_buy_listing_settles_the_split() {
        let env         = Env::default();
        let contract_id = env.register(KnowdlyBookContract, ());
        let client      = KnowdlyBookContractClient::new(&env, &contract_id);
        let platform    = Address::generate(&env);
        let creator     = Address::generate(&env);
        let seller      = Address::generate(&env);
        let buyer       = Address::generate(&env);

        env.mock_all_auths();
        client.initialise(&platform, &250u32);
        let asset = payment_asset(&env, &client, &platform);
        fund(&env, &asset, &seller, 100_000_000);
        fund(&env, &asset, &buyer,  100_000_000);

        let book_id = client.register_book(
            &creator,
            &10_000_000i128,
            &500u32,
            &String::from_str(&env, "arweave-tx-resale"),
            &String::from_str(&env, "Resale Split Book"),
        );

        let token_id = client.purchase(&seller, &book_id);
        let creator_after_primary = balance(&env, &asset, &creator);

        client.list_for_sale(&seller, &token_id, &8_000_000i128);
        client.buy_listing(&buyer, &token_id);

        // 8_000_000 split: 500 bps royalty, 250 bps platform fee, rest to seller
        assert_eq!(balance(&env, &asset, &buyer), 92_000_000);
        assert_eq!(
            balance(&env, &asset, &creator),
            creator_after_primary + 400_000,
        );
        assert_eq!(client.owns_book(&buyer, &book_id), true);
    }

    #[test]
    #[should_panic]
    fn test_buy_listing_without_funds_fails() {
        let env         = Env::default();
        let contract_id = env.register(KnowdlyBookContract, ());
        let client      = KnowdlyBookContractClient::new(&env, &contract_id);
        let platform    = Address::generate(&env);
        let creator     = Address::generate(&env);
        let seller      = Address::generate(&env);
        let pauper      = Address::generate(&env);

        env.mock_all_auths();
        client.initialise(&platform, &250u32);
        let asset = payment_asset(&env, &client, &platform);
        fund(&env, &asset, &seller, 100_000_000);

        let book_id = client.register_book(
            &creator,
            &10_000_000i128,
            &500u32,
            &String::from_str(&env, "arweave-tx-free"),
            &String::from_str(&env, "Not Free Book"),
        );

        let token_id = client.purchase(&seller, &book_id);
        client.list_for_sale(&seller, &token_id, &8_000_000i128);

        // an unfunded observer calling buy_listing on a live listing used to
        // take the token for nothing; now the transfer traps
        client.buy_listing(&pauper, &token_id);
    }

    #[test]
    #[should_panic]
    fn test_initialise_cannot_be_called_twice() {
        let env         = Env::default();
        let contract_id = env.register(KnowdlyBookContract, ());
        let client      = KnowdlyBookContractClient::new(&env, &contract_id);
        let platform    = Address::generate(&env);
        let attacker    = Address::generate(&env);

        env.mock_all_auths();
        client.initialise(&platform, &250u32);

        // re-initialising would hand the attacker the platform role, and with
        // it upgrade(), plus reset the id counters onto existing records
        client.initialise(&attacker, &0u32);
    }

    #[test]
    #[should_panic]
    fn test_buy_listing_rejects_a_second_copy() {
        let env         = Env::default();
        let contract_id = env.register(KnowdlyBookContract, ());
        let client      = KnowdlyBookContractClient::new(&env, &contract_id);
        let platform    = Address::generate(&env);
        let creator     = Address::generate(&env);
        let reader_a    = Address::generate(&env);
        let reader_b    = Address::generate(&env);

        env.mock_all_auths();
        client.initialise(&platform, &250u32);
        let asset = payment_asset(&env, &client, &platform);
        fund(&env, &asset, &reader_a, 100_000_000);
        fund(&env, &asset, &reader_b, 100_000_000);

        let book_id = client.register_book(
            &creator,
            &10_000_000i128,
            &500u32,
            &String::from_str(&env, "arweave-tx-dupe"),
            &String::from_str(&env, "Duplicate Copy Book"),
        );

        let token_a = client.purchase(&reader_a, &book_id);
        client.purchase(&reader_b, &book_id);

        // reader_b already owns this book; a second copy would make the single
        // Ownership bool wrong the moment either copy moves on
        client.list_for_sale(&reader_a, &token_a, &8_000_000i128);
        client.buy_listing(&reader_b, &token_a);
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
        let asset = payment_asset(&env, &client, &platform);
        fund(&env, &asset, &seller, 100_000_000);

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