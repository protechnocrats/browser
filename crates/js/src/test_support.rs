//! Constructors for tests: an environment with a private network context
//! and storage store, granted both. They mint capabilities, so they are
//! compiled only for this crate's tests and for crates that enable the
//! `test-support` feature from a dev-dependency (or the fuzz workspace) —
//! never into a production build (`scripts/check-capabilities.sh`; ADR 010
//! §2). Real pages are built with [`JsEnvironment::new_with_capabilities`].

use super::*;
use network::Origin;

impl JsEnvironment {
    /// A document with no URL: a fresh **opaque origin** (ADR 004), so it can
    /// read only its own responses and cross-origin ones that send
    /// `Access-Control-Allow-Origin: *`, its requests are cross-site to
    /// everything, and its storage is private.
    pub fn new(document: Rc<RefCell<Document>>) -> Self {
        Self::new_with_page_url(document, None)
    }

    /// A document loaded from `page_url` (`None`: see [`Self::new`]), with a
    /// private network context.
    pub fn new_with_page_url(document: Rc<RefCell<Document>>, page_url: Option<&str>) -> Self {
        Self::new_with_network(document, page_url, network::NetworkContext::new())
    }

    /// Like [`Self::new_with_page_url`], granted `network` (and a private
    /// storage store).
    pub fn new_with_network(document: Rc<RefCell<Document>>, page_url: Option<&str>, network: network::NetworkContext) -> Self {
        let page = match page_url {
            Some(url) => Initiator::for_url(url),
            None => Initiator::document(Origin::new_opaque(), None),
        };
        Self::new_with_store(document, page, network, &storage::StorageStore::new())
    }

    /// Granted `network` and `store`, the storage narrowed to `page`'s own
    /// storage key — what the engine does for a real page.
    pub fn new_with_store(
        document: Rc<RefCell<Document>>,
        page: Initiator,
        network: network::NetworkContext,
        store: &storage::StorageStore,
    ) -> Self {
        let authority = capability::Authority::new();
        let key = origin::StorageKey::first_party(&page.origin);
        let capabilities = ScriptCapabilities {
            network: Some(authority.network(network)),
            local_storage: Some(authority.storage(store.clone()).scope(&key)),
            fingerprint: privacy::FingerprintPolicy::standard(),
        };
        Self::new_with_capabilities(document, page, capabilities)
    }
}
