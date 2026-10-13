use std::sync::{Arc, Mutex};

use sx_core::HandlerRegistry;
use sx_plugin_api::{
    stabby, SxBytes, SxHandlerInfo, SxNamespacePolicy, SxQueryContext, SxResult, SxResultSender,
};

// ── MockHandler ───────────────────────────────────────────────────────────────

struct MockHandler {
    namespace: &'static str,
    aliases: Vec<&'static str>,
    policy: SxNamespacePolicy,
    file_extensions: Vec<&'static str>,
    file_extensions_optional: Vec<&'static str>,
}

impl MockHandler {
    fn new(namespace: &'static str, policy: SxNamespacePolicy) -> Self {
        Self {
            namespace,
            aliases: vec![],
            policy,
            file_extensions: vec![],
            file_extensions_optional: vec![],
        }
    }
    fn exclusive(ns: &'static str) -> Self {
        Self::new(ns, SxNamespacePolicy::Exclusive)
    }
    fn shared(ns: &'static str) -> Self {
        Self::new(ns, SxNamespacePolicy::Shared)
    }
    fn with_aliases(mut self, aliases: Vec<&'static str>) -> Self {
        self.aliases = aliases;
        self
    }
    fn with_extensions(mut self, exts: Vec<&'static str>) -> Self {
        self.file_extensions = exts;
        self
    }
    fn with_optional_extensions(mut self, exts: Vec<&'static str>) -> Self {
        self.file_extensions_optional = exts;
        self
    }
}

impl sx_plugin_api::SxHandler for MockHandler {
    extern "C" fn info(&self) -> SxHandlerInfo {
        let mut aliases = stabby::vec::Vec::new();
        for a in &self.aliases {
            aliases.push(SxBytes::from_str_bytes(a));
        }
        let mut file_extensions = stabby::vec::Vec::new();
        for e in &self.file_extensions {
            file_extensions.push(SxBytes::from_str_bytes(e));
        }
        let mut file_extensions_optional = stabby::vec::Vec::new();
        for e in &self.file_extensions_optional {
            file_extensions_optional.push(SxBytes::from_str_bytes(e));
        }
        SxHandlerInfo {
            handler_version: SxBytes::from_str_bytes("0.1.0"),
            namespace: SxBytes::from_str_bytes(self.namespace),
            aliases,
            description: SxBytes::from_str_bytes("test handler"),
            file_extensions,
            file_extensions_optional,
            namespace_policy: self.policy.clone(),
            global_scope: false,
        }
    }

    extern "C" fn query(
        &self,
        value: stabby::str::Str<'_>,
        _ctx: &SxQueryContext,
        sender: SxResultSender,
    ) {
        use sx_plugin_api::meta_text;
        let mut result = SxResult::new(self.namespace, 1.0);
        meta_text(&mut result, "value", value.as_ref());
        sender.send(result);
    }

    extern "C" fn prepare_extract(
        &self,
    ) -> stabby::option::Option<sx_plugin_api::SxExtractorHandle> {
        stabby::option::Option::None()
    }
}

fn arc(h: MockHandler) -> Arc<dyn sx_plugin_api::SxHandler> {
    Arc::new(h)
}

fn collecting_sender() -> (SxResultSender, Arc<Mutex<Vec<String>>>) {
    let results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r2 = Arc::clone(&results);
    let sender = SxResultSender::from_fn(move |res: SxResult| {
        // Collect the "value" metadata entry the mock handler stores.
        let val = res
            .metadata
            .iter()
            .find(|e| e.key.as_str() == "value")
            .map(|e| e.value.match_ref(|t| t.as_str().to_owned(), |n| n.to_string(), |_| String::new()));
        if let Some(s) = val {
            r2.lock().unwrap().push(s);
        }
        true
    });
    (sender, results)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[test]
fn register_and_dispatch_basic() {
    let mut reg = HandlerRegistry::new();
    reg.register(arc(MockHandler::exclusive("weather")))
        .unwrap();

    let ctx = SxQueryContext::for_test();
    let (sender, results) = collecting_sender();
    reg.dispatch(b"weather", "berlin", &ctx, sender).unwrap();
    assert_eq!(*results.lock().unwrap(), vec!["berlin"]);
}

#[test]
fn exclusive_first_wins_second_is_error() {
    let mut reg = HandlerRegistry::new();
    reg.register(arc(MockHandler::exclusive("weather")))
        .unwrap();
    let err = reg
        .register(arc(MockHandler::exclusive("weather")))
        .unwrap_err();
    assert_eq!(err.ns.as_ref(), "weather");
}

#[test]
fn exclusive_does_not_prevent_different_namespace() {
    let mut reg = HandlerRegistry::new();
    reg.register(arc(MockHandler::exclusive("weather")))
        .unwrap();
    reg.register(arc(MockHandler::exclusive("news"))).unwrap();
    assert!(reg.knows_namespace(b"weather"));
    assert!(reg.knows_namespace(b"news"));
}

#[test]
fn unknown_namespace_dispatch_returns_error() {
    let reg = HandlerRegistry::new();
    let ctx = SxQueryContext::for_test();
    let (sender, _) = collecting_sender();
    let err = reg
        .dispatch(b"weather", "berlin", &ctx, sender)
        .unwrap_err();
    assert_eq!(err.0.as_ref(), "weather");
}

#[test]
fn knows_namespace() {
    let mut reg = HandlerRegistry::new();
    reg.register(arc(MockHandler::exclusive("weather")))
        .unwrap();
    assert!(reg.knows_namespace(b"weather"));
    assert!(!reg.knows_namespace(b"news"));
}

#[test]
fn alias_dispatches_to_handler() {
    let mut reg = HandlerRegistry::new();
    reg.register(arc(
        MockHandler::exclusive("weather").with_aliases(vec!["w"])
    ))
    .unwrap();

    let ctx = SxQueryContext::for_test();
    let (sender, results) = collecting_sender();
    reg.dispatch(b"w", "london", &ctx, sender).unwrap();
    assert_eq!(*results.lock().unwrap(), vec!["london"]);
}

#[test]
fn duplicate_alias_second_is_skipped() {
    let mut reg = HandlerRegistry::new();
    reg.register(arc(MockHandler::exclusive("foo").with_aliases(vec!["x"])))
        .unwrap();
    // Second handler — different primary, same alias.
    reg.register(arc(MockHandler::exclusive("bar").with_aliases(vec!["x"])))
        .unwrap();

    assert!(reg.knows_namespace(b"foo"));
    assert!(reg.knows_namespace(b"bar"));

    // Alias "x" stays with first; dispatch via "x" yields exactly one result.
    let ctx = SxQueryContext::for_test();
    let (sender, results) = collecting_sender();
    reg.dispatch(b"x", "test", &ctx, sender).unwrap();
    assert_eq!(results.lock().unwrap().len(), 1);
}

#[test]
fn shared_namespace_dispatches_all_handlers() {
    let mut reg = HandlerRegistry::new();
    reg.register(arc(MockHandler::shared("meta"))).unwrap();
    reg.register(arc(MockHandler::shared("meta"))).unwrap();

    let ctx = SxQueryContext::for_test();
    let (sender, results) = collecting_sender();
    reg.dispatch(b"meta", "location", &ctx, sender).unwrap();
    assert_eq!(results.lock().unwrap().len(), 2);
}

#[test]
fn exclusive_second_registration_of_shared_is_also_error() {
    // The stored policy of the *first* registrant governs. First is Shared →
    // the second handler is appended regardless of its own declared policy.
    let mut reg = HandlerRegistry::new();
    reg.register(arc(MockHandler::shared("meta"))).unwrap();
    reg.register(arc(MockHandler::exclusive("meta"))).unwrap();

    let ctx = SxQueryContext::for_test();
    let (sender, results) = collecting_sender();
    reg.dispatch(b"meta", "ping", &ctx, sender).unwrap();
    // Both handlers respond.
    assert_eq!(results.lock().unwrap().len(), 2);
}

#[test]
fn default_and_optional_file_extensions() {
    let mut reg = HandlerRegistry::new();
    // txt and pdf are default-on.
    reg.register(arc(
        MockHandler::exclusive("txt-plugin").with_extensions(vec!["txt", "md"])
    ))
    .unwrap();
    reg.register(arc(
        MockHandler::exclusive("pdf-plugin").with_extensions(vec!["pdf"])
    ))
    .unwrap();
    // gz is optional — should NOT appear in default list but SHOULD appear in all.
    reg.register(arc(MockHandler::exclusive("man-plugin")
        .with_extensions(vec!["1", "2", "3"])
        .with_optional_extensions(vec!["gz"])))
        .unwrap();

    let defaults = reg.default_file_extensions();
    assert!(defaults.contains(b"txt".as_slice()), "txt is default-on");
    assert!(defaults.contains(b"md".as_slice()), "md is default-on");
    assert!(defaults.contains(b"pdf".as_slice()), "pdf is default-on");
    assert!(
        defaults.contains(b"1".as_slice()),
        "man section 1 is default-on"
    );
    assert!(
        !defaults.contains(b"gz".as_slice()),
        "gz is opt-in, not default"
    );

    let all = reg.all_file_extensions();
    assert!(all.contains(b"txt".as_slice()), "txt in all");
    assert!(all.contains(b"gz".as_slice()), "gz in all (optional)");
    assert!(
        !defaults.contains(b"gz".as_slice()),
        "gz still absent from defaults"
    );
}

#[test]
fn calc_handler_registers_and_dispatches() {
    use sx_plugin_calc::CalcHandler;
    let mut reg = HandlerRegistry::new();
    reg.register(Arc::new(CalcHandler)).unwrap();

    let err = reg.register(Arc::new(CalcHandler)).unwrap_err();
    assert_eq!(err.ns.as_ref(), "calc");

    let ctx = SxQueryContext::for_test();
    let (sender, results) = collecting_sender();
    reg.dispatch(b"calc", "2^10", &ctx, sender).unwrap();
    assert_eq!(*results.lock().unwrap(), vec!["1024"]);
}
