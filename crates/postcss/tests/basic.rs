use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use postcss::input::{Input, InputOptions, Position};
use postcss::processor::ProcessorError;
use postcss::source_map::MapOptions;
use postcss::{
    ast::nodes::{
        as_comment, as_declaration, as_rule, Declaration, Root as AstRoot, RootLike,
        Rule as AstRule,
    },
    ast::NodeAccess,
    at_rule as at_rule_ctor, comment as comment_ctor, decl as decl_ctor, document as document_ctor,
    from_json, parse, postcss as postcss_fn, postcss_with_plugins,
    processor::{IntoPlugin, IntoStringifier, PluginBuilder},
    root as root_ctor, rule as rule_ctor, stringify, to_json, Plugin, PositionByOptions,
    ProcessOptions, Processor, RangeByOptions, ReplaceValuesOptions, Result as PostcssResult,
    WarningOptions,
};
use regex::Regex;

fn noop_raw_waker() -> RawWaker {
    unsafe fn clone(_: *const ()) -> RawWaker {
        noop_raw_waker()
    }
    unsafe fn wake(_: *const ()) {}
    unsafe fn wake_by_ref(_: *const ()) {}
    unsafe fn drop(_: *const ()) {}

    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    RawWaker::new(std::ptr::null(), &VTABLE)
}

fn block_on<F>(future: F) -> F::Output
where
    F: Future,
{
    let mut future = future;
    let waker = unsafe { Waker::from_raw(noop_raw_waker()) };
    let mut context = Context::from_waker(&waker);
    let mut pinned = unsafe { Pin::new_unchecked(&mut future) };

    loop {
        match pinned.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => continue,
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct JsJsonResult {
    json: serde_json::Value,
    rehydrated: String,
}

fn run_js_ast(css: &str, body: &str) -> JsJsonResult {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let js_path = manifest_dir
        .join("../../lib/postcss.js")
        .canonicalize()
        .expect("canonicalize postcss path");
    let require_path = serde_json::to_string(
        js_path
            .to_str()
            .expect("postcss.js path must be valid utf-8"),
    )
    .expect("serialize require path");

    let script = format!(
        "const fs=require('fs');const postcss=require({require_path});const css=fs.readFileSync(0,'utf8');{body}",
    );

    let mut child = Command::new("node")
        .arg("-e")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn node");

    {
        let stdin = child.stdin.as_mut().expect("node stdin available");
        stdin
            .write_all(css.as_bytes())
            .expect("write css to node stdin");
    }

    let output = child.wait_with_output().expect("wait for node to finish");

    assert!(
        output.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    serde_json::from_slice(&output.stdout).expect("parse node output")
}

fn run_js_root_json(css: &str) -> JsJsonResult {
    run_js_ast(
        css,
        "const root=postcss.parse(css);const json=root.toJSON();const rehydrated=postcss.fromJSON(json).toString();console.log(JSON.stringify({json,rehydrated}));",
    )
}

fn run_js_decl_json(css: &str) -> JsJsonResult {
    run_js_ast(
        css,
        "const root=postcss.parse(css);const decl=root.first.nodes[0];const json=decl.toJSON();const rehydrated=postcss.fromJSON(json).toString();console.log(JSON.stringify({json,rehydrated}));",
    )
}

fn normalize_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.remove("raws");
            map.remove("source");
            if let Some(inputs) = map.get_mut("inputs") {
                if let serde_json::Value::Array(entries) = inputs {
                    for entry in entries {
                        if let serde_json::Value::Object(obj) = entry {
                            obj.remove("id");
                            obj.remove("hasBOM");
                        }
                    }
                }
            }
            for value in map.values_mut() {
                normalize_json(value);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                normalize_json(item);
            }
        }
        _ => {}
    }
}

struct UppercasePlugin;

impl Plugin for UppercasePlugin {
    fn name(&self) -> &str {
        "uppercase"
    }

    fn visit_rule(
        &self,
        rule: &AstRule,
        _result: &mut PostcssResult,
    ) -> Result<(), ProcessorError> {
        rule.set_selector(rule.selector().to_uppercase());
        Ok(())
    }
}

fn selectors(root: &AstRoot) -> Vec<String> {
    root.nodes()
        .into_iter()
        .filter_map(|node| as_rule(&node))
        .map(|rule| rule.selector())
        .collect()
}

#[test]
fn parse_and_stringify_roundtrip() {
    let css = "a { color: black; }";
    let root = parse(css).expect("parse");
    let output = stringify(&root);
    assert!(output.contains("color"));
}

#[test]
fn stringifier_formats_manual_ast() {
    let root = AstRoot::new();
    let rule = AstRule::new("a");
    let decl = Declaration::new("color", "red");
    rule.append(decl.to_node());
    root.append(rule.to_node());

    let css = stringify(&root);
    assert!(css.contains("a"));
    assert!(css.contains(":"));
    assert!(css.contains("color"));
}

#[test]
fn convenience_constructors_match_ast_builders() {
    let root = root_ctor();
    let document = document_ctor();
    let rule = rule_ctor("a");
    let at = at_rule_ctor("media");
    let decl = decl_ctor("color", "black");
    let comment = comment_ctor("test");

    assert!(matches!(
        root.to_node().borrow().kind(),
        postcss::ast::nodes::NodeKind::Root
    ));
    assert!(matches!(
        document.to_node().borrow().kind(),
        postcss::ast::nodes::NodeKind::Document
    ));
    assert_eq!(rule.selector(), "a");
    assert_eq!(at.params(), "");
    assert_eq!(decl.prop(), "color");
    assert_eq!(decl.value(), "black");
    assert_eq!(comment.text(), "test");
}

#[test]
fn container_each_handles_node_removal() {
    let root = AstRoot::new();
    let first = AstRule::new("a");
    let second = AstRule::new("b");
    root.append(first.to_node());
    root.append(second.to_node());

    let mut selectors = Vec::new();
    root.each(|node, index| {
        let rule = as_rule(&node).expect("rule");
        selectors.push(rule.selector());
        root.remove_child(index)
            .expect("remove existing child by index");
        true
    });

    assert_eq!(selectors, vec!["a", "b"]);
    assert!(root.is_empty());
}

#[test]
fn replace_values_replaces_literal_patterns() {
    let css = "a{one:1}b{two:1 2}";
    let root = parse(css).expect("parse css");
    let returned = root.replace_values("1", "A");

    let returned_node = returned.to_node();
    let root_node = root.to_node();
    assert!(Rc::ptr_eq(&returned_node, &root_node));
    assert_eq!(root.to_css(), "a{one:A}b{two:A 2}");
}

#[test]
fn replace_values_supports_regex_callbacks() {
    let css = "a{one:1}b{two:1 2}";
    let root = parse(css).expect("parse css");
    let regex = Regex::new("\\d").expect("compile regex");

    let _ = root.replace_values(regex, |substring: &str| format!("{substring}A"));

    assert_eq!(root.to_css(), "a{one:1A}b{two:1A 2A}");
}

#[test]
fn replace_values_respects_props_and_fast_options() {
    let css = "a{one:1}b{two:1 2}";
    let first = parse(css).expect("parse css for props");
    let options = ReplaceValuesOptions::new().props(["one"]);
    let _ = first.replace_values_with_options("1", options, "A");
    assert_eq!(first.to_css(), "a{one:A}b{two:1 2}");

    let second = parse(css).expect("parse css for fast option");
    let options = ReplaceValuesOptions::new().fast("2");
    let _ = second.replace_values_with_options("1", options, "A");
    assert_eq!(second.to_css(), "a{one:1}b{two:A 2}");
}

#[test]
fn container_every_and_some_match_predicates() {
    let css = "a { a: 1; b: 2 }";
    let root = parse(css).expect("parse css");
    let first_node = root.first().expect("rule node");
    let rule = as_rule(&first_node).expect("rule");

    let ab_ok = rule.every(|node, _| {
        as_declaration(&node)
            .map(|decl| {
                let prop = decl.prop();
                prop == "a" || prop == "b"
            })
            .unwrap_or(false)
    });
    assert!(
        ab_ok,
        "rule.every should accept predicates matching all declarations"
    );

    let only_b = rule.every(|node, _| {
        as_declaration(&node)
            .map(|decl| decl.prop() == "b")
            .unwrap_or(false)
    });
    assert!(
        !only_b,
        "rule.every should reject when any declaration fails predicate"
    );

    let some_b = rule.some(|node, _| {
        as_declaration(&node)
            .map(|decl| decl.prop() == "b")
            .unwrap_or(false)
    });
    assert!(
        some_b,
        "rule.some should succeed when any declaration matches predicate"
    );

    let some_c = rule.some(|node, _| {
        as_declaration(&node)
            .map(|decl| decl.prop() == "c")
            .unwrap_or(false)
    });
    assert!(
        !some_c,
        "rule.some should fail when no declaration matches predicate"
    );
}

#[test]
fn container_index_and_remove_child_accept_nodes() {
    let css = "a { a: 1; b: 2 }";
    let root = parse(css).expect("parse css");
    let rule_node = root.first().expect("rule node");
    let rule = as_rule(&rule_node).expect("rule");

    let first_decl = rule.first().expect("first declaration node");
    let second_decl = rule.last().expect("last declaration node");

    assert_eq!(rule.child_index(0), Some(0));
    assert_eq!(rule.child_index(&first_decl), Some(0));
    assert_eq!(rule.child_index(&second_decl), Some(1));

    let removed = rule
        .remove_child(&second_decl)
        .expect("remove declaration by node reference");
    assert!(Rc::ptr_eq(&removed, &second_decl));
    let removed_decl = as_declaration(&removed).expect("removed declaration");
    assert!(removed_decl.parent().is_none());

    rule.remove_all();
    assert!(rule.first().is_none());
    let decl = as_declaration(&first_decl).expect("first declaration");
    assert!(decl.parent().is_none());
}
#[test]
fn walk_rules_if_filters_selectors() {
    let css = "a {}\nb {}\nc {}";
    let root = parse(css).expect("parse");
    let mut selectors = Vec::new();
    root.walk_rules_if(
        |selector| selector.contains('b'),
        |node, _| {
            let rule = as_rule(&node).expect("rule");
            selectors.push(rule.selector());
            true
        },
    );

    assert_eq!(selectors, vec!["b"]);
}

#[test]
fn postcss_function_accepts_plugin_iterables() {
    struct Uppercase;

    impl Plugin for Uppercase {
        fn name(&self) -> &str {
            "uppercase"
        }

        fn visit_rule(
            &self,
            rule: &AstRule,
            _result: &mut PostcssResult,
        ) -> Result<(), ProcessorError> {
            rule.set_selector(rule.selector().to_uppercase());
            Ok(())
        }
    }

    let processor = postcss_with_plugins([Uppercase]);
    let css = "a { color: black }";
    let mut result = processor.process(css).expect("process");
    let root = result
        .root()
        .expect("root result access")
        .expect("root should exist")
        .clone();
    let selectors: Vec<_> = root
        .nodes()
        .into_iter()
        .filter_map(|node| as_rule(&node))
        .map(|rule| rule.selector())
        .collect();
    assert_eq!(selectors, vec!["A"], "selectors: {:?}", selectors);
}

#[test]
fn plugin_builder_runs_all_hooks() {
    let run_calls = Arc::new(Mutex::new(0usize));
    let visit_rules = Arc::new(Mutex::new(Vec::new()));

    let plugin = PluginBuilder::new("builder")
        .run({
            let counter = Arc::clone(&run_calls);
            move |result| {
                let mut guard = counter.lock().unwrap();
                *guard += 1;
                result.warn("run", WarningOptions::new());
                Ok(())
            }
        })
        .rule({
            let seen = Arc::clone(&visit_rules);
            move |rule, _| {
                seen.lock().unwrap().push(rule.selector());
                rule.set_selector(format!("{}-visited", rule.selector()));
                Ok(())
            }
        })
        .build();

    let processor = postcss_fn().with_plugin(plugin);
    let css = "a { color: black }";
    let mut result = processor.process(css).expect("process");

    let warnings = result.warnings().expect("warnings");
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].text, "run");

    let css_output = result.css().expect("css");
    assert!(
        css_output.contains("a-visited"),
        "css_output: {}",
        css_output
    );

    assert_eq!(*run_calls.lock().unwrap(), 1);
    assert_eq!(visit_rules.lock().unwrap().as_slice(), &["a".to_string()]);
}

#[test]
fn plugin_prepare_produces_per_result_plugin_instances() {
    let prepare_calls = Arc::new(Mutex::new(0usize));
    let seen_selectors = Arc::new(Mutex::new(Vec::new()));

    let plugin = PluginBuilder::new("prepared")
        .prepare({
            let prepare_calls = Arc::clone(&prepare_calls);
            let seen_selectors = Arc::clone(&seen_selectors);
            move |result: &mut PostcssResult| {
                *prepare_calls.lock().unwrap() += 1;
                result.warn("from prepare", WarningOptions::new());

                let seen_selectors = Arc::clone(&seen_selectors);
                let prepared = PluginBuilder::new("prepared-child")
                    .rule(move |rule, _| {
                        seen_selectors.lock().unwrap().push(rule.selector());
                        Ok(())
                    })
                    .into_plugin();

                Ok(Some(prepared))
            }
        })
        .build();

    let processor = postcss_fn().with_plugin(plugin);
    let css = "a { color: black }";
    let mut result = processor.process(css).expect("process");

    let warnings = result.warnings().expect("warnings produced");
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].text, "from prepare");

    assert_eq!(*prepare_calls.lock().unwrap(), 1);
    assert_eq!(
        seen_selectors.lock().unwrap().as_slice(),
        &["a".to_string()]
    );
}

#[test]
fn filtered_visitors_match_specific_node_keys() {
    let events = Arc::new(Mutex::new(Vec::new()));

    let plugin = PluginBuilder::new("filters")
        .at_rule_filter("media", {
            let events = Arc::clone(&events);
            move |at_rule, _| {
                events
                    .lock()
                    .unwrap()
                    .push(format!("atrule:{}", at_rule.name()));
                Ok(())
            }
        })
        .rule_filter("*", {
            let events = Arc::clone(&events);
            move |rule, _| {
                events
                    .lock()
                    .unwrap()
                    .push(format!("rule:*:{}", rule.selector()));
                Ok(())
            }
        })
        .rule_filter("b", {
            let events = Arc::clone(&events);
            move |rule, _| {
                events
                    .lock()
                    .unwrap()
                    .push(format!("rule:b:{}", rule.selector()));
                Ok(())
            }
        })
        .rule_filter_exit("b", {
            let events = Arc::clone(&events);
            move |rule, _| {
                events
                    .lock()
                    .unwrap()
                    .push(format!("rule-exit:b:{}", rule.selector()));
                Ok(())
            }
        })
        .decl_filter("color", {
            let events = Arc::clone(&events);
            move |decl, _| {
                events
                    .lock()
                    .unwrap()
                    .push(format!("decl:{}:{}", decl.prop(), decl.value()));
                decl.set_value("blue");
                Ok(())
            }
        })
        .decl_filter_exit("margin", {
            let events = Arc::clone(&events);
            move |decl, _| {
                events
                    .lock()
                    .unwrap()
                    .push(format!("decl-exit:margin:{}", decl.value()));
                Ok(())
            }
        })
        .build();

    let processor = postcss_fn().with_plugin(plugin);
    let css = "@media screen { a { color: black; margin: 0 } b { color: white } }";
    let mut result = processor.process(css).expect("process");

    let root = result
        .root()
        .expect("root result access")
        .expect("root should exist")
        .clone();
    let mut colors = Vec::new();
    root.walk_decls_if(
        |prop| prop == "color",
        |node, _| {
            let decl = as_declaration(&node).expect("decl");
            colors.push(decl.value());
            true
        },
    );
    assert_eq!(colors, vec!["blue".to_string(), "blue".to_string()]);

    let recorded = events.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec![
            "atrule:media".to_string(),
            "rule:*:a".to_string(),
            "decl:color:black".to_string(),
            "decl-exit:margin:0".to_string(),
            "rule:*:b".to_string(),
            "rule:b:b".to_string(),
            "decl:color:white".to_string(),
            "rule-exit:b:b".to_string(),
        ]
    );
}

#[test]
fn node_reordering_matches_postcss_behaviour() {
    let root = AstRoot::new();
    let rule_a = AstRule::new("a");
    let rule_b = AstRule::new("b");
    let rule_c = AstRule::new("c");

    root.append(rule_a.to_node());
    root.append(rule_c.to_node());

    rule_c.before([rule_b.to_node()]);
    assert_eq!(selectors(&root), vec!["a", "b", "c"]);
    assert_eq!(rule_b.index(), Some(1));
    assert_eq!(
        rule_b
            .next()
            .and_then(|node| as_rule(&node))
            .map(|rule| rule.selector()),
        Some("c".to_string())
    );
    assert_eq!(
        rule_c
            .prev()
            .and_then(|node| as_rule(&node))
            .map(|rule| rule.selector()),
        Some("b".to_string())
    );

    let rule_d = AstRule::new("d");
    rule_a.after([rule_d.to_node()]);
    assert_eq!(selectors(&root), vec!["a", "d", "b", "c"]);

    let rule_e = AstRule::new("e");
    let rule_f = AstRule::new("f");
    rule_d.replace_with([rule_e.to_node(), rule_f.to_node()]);
    assert_eq!(selectors(&root), vec!["a", "e", "f", "b", "c"]);

    let rule_c_ref = rule_c.to_node();
    rule_f.move_after(&rule_c_ref);
    assert_eq!(selectors(&root), vec!["a", "e", "b", "c", "f"]);

    rule_e.move_to(root.raw());
    assert_eq!(selectors(&root), vec!["a", "b", "c", "f", "e"]);

    rule_b.remove();
    assert!(rule_b.parent().is_none());
    assert_eq!(selectors(&root), vec!["a", "c", "f", "e"]);
}

#[test]
fn processor_runs_plugin() {
    let css = "a { color: black; }";
    let processor = Processor::new().with_plugin(UppercasePlugin);
    let mut lazy = processor.process(css).expect("process");
    let output = lazy.css().expect("css").to_string();
    assert!(output.contains("A"), "output: {}", output);
}

#[test]
fn generates_inline_source_map() {
    let processor = Processor::new();
    let mut map_opts = MapOptions::default();
    map_opts.inline = Some(true);
    let options = ProcessOptions::new()
        .from_path("input.css")
        .to_path("output.css")
        .enable_map_with(map_opts);
    let mut lazy = processor
        .process_with_options("a { color: black; }", options)
        .expect("process");
    let css = lazy.css().expect("css").to_string();
    let mut result = lazy.into_result().expect("result");
    assert!(css.contains("sourceMappingURL=data:application/json;base64,"));
    assert!(result.map().is_none());
}

#[test]
fn generates_external_source_map() {
    let processor = Processor::new();
    let mut map_opts = MapOptions::default();
    map_opts.inline = Some(false);
    let options = ProcessOptions::new()
        .from_path("input.css")
        .to_path("output.css")
        .enable_map_with(map_opts);
    let mut lazy = processor
        .process_with_options("a { color: black; }", options)
        .expect("process");
    let css = lazy.css().expect("css").to_string();
    let mut result = lazy.into_result().expect("result");
    assert!(css.contains("sourceMappingURL=output.css.map"));
    let map = result.map().expect("map");
    assert!(map.contains("\"version\":3"));
}

struct WarningPlugin;

impl Plugin for WarningPlugin {
    fn name(&self) -> &str {
        "warning"
    }

    fn run(&self, result: &mut PostcssResult) -> Result<(), ProcessorError> {
        result.warn("problem", WarningOptions::new());
        Ok(())
    }
}

struct VisitorRecorder {
    events: Arc<Mutex<Vec<String>>>,
}

impl VisitorRecorder {
    fn new(events: Arc<Mutex<Vec<String>>>) -> Self {
        Self { events }
    }
}

impl Plugin for VisitorRecorder {
    fn name(&self) -> &str {
        "visitor"
    }

    fn once(
        &self,
        root: &postcss::ast::nodes::RootLike,
        _result: &mut PostcssResult,
    ) -> Result<(), ProcessorError> {
        let is_empty = root.as_root().expect("once should receive root").is_empty();
        self.events
            .lock()
            .unwrap()
            .push(format!("once:{}", is_empty));
        Ok(())
    }

    fn visit_root(
        &self,
        _root: &AstRoot,
        _result: &mut PostcssResult,
    ) -> Result<(), ProcessorError> {
        self.events.lock().unwrap().push("root".into());
        Ok(())
    }

    fn visit_root_exit(
        &self,
        _root: &AstRoot,
        _result: &mut PostcssResult,
    ) -> Result<(), ProcessorError> {
        self.events.lock().unwrap().push("rootExit".into());
        Ok(())
    }

    fn visit_rule(
        &self,
        rule: &AstRule,
        _result: &mut PostcssResult,
    ) -> Result<(), ProcessorError> {
        self.events
            .lock()
            .unwrap()
            .push(format!("rule:{}", rule.selector()));
        Ok(())
    }

    fn visit_rule_exit(
        &self,
        rule: &AstRule,
        _result: &mut PostcssResult,
    ) -> Result<(), ProcessorError> {
        self.events
            .lock()
            .unwrap()
            .push(format!("ruleExit:{}", rule.selector()));
        Ok(())
    }

    fn visit_declaration(
        &self,
        decl: &Declaration,
        _result: &mut PostcssResult,
    ) -> Result<(), ProcessorError> {
        self.events
            .lock()
            .unwrap()
            .push(format!("decl:{}", decl.prop()));
        Ok(())
    }

    fn visit_declaration_exit(
        &self,
        decl: &Declaration,
        _result: &mut PostcssResult,
    ) -> Result<(), ProcessorError> {
        self.events
            .lock()
            .unwrap()
            .push(format!("declExit:{}", decl.prop()));
        Ok(())
    }

    fn once_exit(
        &self,
        _root: &RootLike,
        _result: &mut PostcssResult,
    ) -> Result<(), ProcessorError> {
        self.events.lock().unwrap().push("onceExit".into());
        Ok(())
    }
}

#[test]
fn warns_with_plugin_name_by_default() {
    let processor = Processor::new().with_plugin(WarningPlugin);
    let mut lazy = processor.process("a{}").expect("process");
    lazy.css().expect("css");
    assert_eq!(processor.version(), "8.4.31");
    let result = lazy.into_result().expect("result");
    assert_eq!(result.warnings.len(), 1);
    let warning = &result.warnings[0];
    assert_eq!(warning.plugin.as_deref(), Some("warning"));
    assert_eq!(warning.message_type(), "warning");
    assert_eq!(result.processor.version(), "8.4.31");
    assert_eq!(result.messages().len(), 1);
    assert_eq!(result.messages()[0].message_type(), "warning");
}

#[test]
fn plugin_visitors_traverse_tree() {
    let css = "a { color: black; }";
    let events = Arc::new(Mutex::new(Vec::new()));
    let plugin = VisitorRecorder::new(Arc::clone(&events));
    let processor = Processor::new().with_plugin(plugin);
    let mut lazy = processor.process(css).expect("process");
    lazy.css().expect("css");
    let recorded = events.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec![
            "once:false",
            "root",
            "rule:a",
            "decl:color",
            "declExit:color",
            "ruleExit:a",
            "rootExit",
            "onceExit",
        ]
    );
}

#[test]
fn processor_without_plugins_returns_no_work_result() {
    let css = "a { color: black; }";
    let mut result = Processor::new().process(css).expect("process");
    assert!(
        result.is_no_work(),
        "expected no-work result when no plugins or custom syntax are provided"
    );

    let css_output = result.css().expect("css").to_string();
    assert!(css_output.contains("color: black"));

    {
        let warnings = result.warnings().expect("warnings");
        assert!(warnings.is_empty());
    }

    {
        let root = result
            .root()
            .expect("root result access")
            .expect("root should exist");
        assert!(!root.is_empty());
    }
}

#[test]
fn lazy_result_exposes_processed_state() {
    let mut map_opts = MapOptions::default();
    map_opts.inline = Some(false);
    let options = ProcessOptions::new()
        .from_path("input.css")
        .to_path("output.css")
        .enable_map_with(map_opts);
    let processor = Processor::new().with_plugin(WarningPlugin);
    let mut lazy = processor
        .process_with_options("a { color: black; }", options)
        .expect("process");

    assert_eq!(lazy.processor_version(), "8.4.31");
    {
        let processor = lazy.processor().expect("processor");
        assert_eq!(processor.version(), "8.4.31");
    }

    let css = lazy.css().expect("css").to_string();
    let content = lazy.content().expect("content").to_string();
    assert_eq!(css, content);

    let map = lazy.map().expect("map");
    assert!(map.is_some());

    let warnings_len = {
        let warnings = lazy.warnings().expect("warnings");
        warnings.len()
    };
    assert_eq!(warnings_len, 1);

    let messages_len = {
        let messages = lazy.messages().expect("messages");
        messages.len()
    };
    assert_eq!(messages_len, 1);

    {
        let result = lazy.result().expect("result");
        assert_eq!(result.processor().version(), "8.4.31");
        assert_eq!(result.warnings().len(), 1);
    }

    {
        let root = lazy.root().expect("root result").expect("root value");
        assert!(!root.is_empty());
    }
}

#[test]
fn lazy_result_is_awaitable() {
    let processor = Processor::new();
    let lazy = processor.process("a { color: black; }").expect("process");
    let mut result = block_on(lazy).expect("await result");
    let css = result.css().to_string();
    assert!(css.contains("color"));
}

#[test]
fn root_to_result_matches_processor_output() {
    let css = "a { color: black; }";
    let root = parse(css).expect("parse css into root");

    let mut direct = root.to_result().expect("root to result");
    let direct_css = direct.css().to_string();

    let processor = Processor::new();
    let mut lazy = processor.process(css).expect("process");
    let lazy_css = lazy.css().expect("css").to_string();

    assert_eq!(direct_css, lazy_css);
}

#[test]
fn document_to_result_matches_processor_output() {
    let css = "a { color: black; }";
    let root = parse(css).expect("parse css into root");

    let document = {
        let document = document_ctor();
        document.append(root.clone().to_node());
        document
    };

    let mut direct = document.to_result().expect("document to produce result");
    assert!(direct.document().is_some());
    assert!(direct.root().is_none());
    let direct_css = direct.css().to_string();
    assert_eq!(direct_css, document.to_string());

    let processor = Processor::new();
    let mut lazy = processor
        .process_document(document.clone())
        .expect("process document");
    let lazy_css = lazy.css().expect("css").to_string();

    assert_eq!(lazy_css, direct_css);

    let mut result = lazy.into_result().expect("into result");
    assert!(result.document().is_some());
    assert!(result.root().is_none());
    assert_eq!(result.css().to_string(), direct_css);
}

#[test]
fn processor_uses_custom_parser_when_provided() {
    let css = "a { color: black; }";
    let called = Arc::new(Mutex::new(false));
    let parser_flag = Arc::clone(&called);
    let options = ProcessOptions::new().parser(
        move |input: &str,
              _opts: &ProcessOptions|
              -> Result<postcss::ast::nodes::RootLike, ProcessorError> {
            *parser_flag.lock().unwrap() = true;
            assert_eq!(input, css);
            parse(input)
                .map(postcss::ast::nodes::RootLike::Root)
                .map_err(ProcessorError::from)
        },
    );

    let processor = Processor::new();
    let mut lazy = processor
        .process_with_options(css, options)
        .expect("process with custom parser");
    let _ = lazy.css().expect("css");

    assert!(
        *called.lock().unwrap(),
        "custom parser should be invoked before producing CSS"
    );
}

#[test]
fn processor_uses_custom_stringifier_when_provided() {
    let css = "a { color: black; }";
    let called = Arc::new(Mutex::new(false));
    let flag = Arc::clone(&called);
    let options = ProcessOptions::new().stringifier(
        move |root_like: &postcss::ast::nodes::RootLike,
              builder: &mut dyn FnMut(
            &str,
            Option<&postcss::ast::NodeRef>,
            Option<&'static str>,
        )| {
            *flag.lock().unwrap() = true;
            let root = root_like
                .as_root()
                .expect("custom stringifier should receive root");
            postcss::stringifier::stringify_with_builder(root.raw(), &mut |chunk: &str,
                                                                           node: Option<
                &postcss::ast::NodeRef,
            >,
                                                                           kind: Option<
                &'static str,
            >| {
                builder(&chunk.to_uppercase(), node, kind);
            });
        },
    );

    let processor = Processor::new();
    let mut lazy = processor
        .process_with_options(css, options)
        .expect("process with custom stringifier");
    let output = lazy.css().expect("css").to_string();

    assert_eq!(output, "A { COLOR: BLACK; }");
    assert!(
        *called.lock().unwrap(),
        "custom stringifier should be invoked during CSS generation"
    );
}

#[test]
fn syntax_option_supplies_parser_and_stringifier_defaults() {
    let css = "a { color: black; }";
    let parser_called = Arc::new(Mutex::new(false));
    let stringifier_called = Arc::new(Mutex::new(false));
    let parser_flag = Arc::clone(&parser_called);
    let stringifier_flag = Arc::clone(&stringifier_called);
    let syntax = postcss::SyntaxOptions::new()
        .parser(
            move |input: &str,
                  _opts: &ProcessOptions|
                  -> Result<postcss::ast::nodes::RootLike, ProcessorError> {
                *parser_flag.lock().unwrap() = true;
                parse(input)
                    .map(postcss::ast::nodes::RootLike::Root)
                    .map_err(ProcessorError::from)
            },
        )
        .stringifier(
            move |root_like: &postcss::ast::nodes::RootLike,
                  builder: &mut dyn FnMut(
                &str,
                Option<&postcss::ast::NodeRef>,
                Option<&'static str>,
            )| {
                *stringifier_flag.lock().unwrap() = true;
                let root = root_like
                    .as_root()
                    .expect("syntax stringifier should receive root");
                postcss::stringifier::stringify_with_builder(root.raw(), builder);
            },
        );
    let options = ProcessOptions::new().syntax(syntax);

    let processor = Processor::new();
    let mut lazy = processor
        .process_with_options(css, options)
        .expect("process with syntax");
    let _ = lazy.css().expect("css");

    assert!(*parser_called.lock().unwrap());
    assert!(*stringifier_called.lock().unwrap());
}

#[test]
fn parser_option_overrides_syntax_parser() {
    let css = "a { color: black; }";
    let syntax_called = Arc::new(Mutex::new(false));
    let parser_called = Arc::new(Mutex::new(false));
    let syntax_flag = Arc::clone(&syntax_called);
    let parser_flag = Arc::clone(&parser_called);
    let syntax = postcss::SyntaxOptions::new().parser(
        move |input: &str,
              _opts: &ProcessOptions|
              -> Result<postcss::ast::nodes::RootLike, ProcessorError> {
            *syntax_flag.lock().unwrap() = true;
            parse(input)
                .map(postcss::ast::nodes::RootLike::Root)
                .map_err(ProcessorError::from)
        },
    );
    let options = ProcessOptions::new().syntax(syntax).parser(
        move |input: &str,
              _opts: &ProcessOptions|
              -> Result<postcss::ast::nodes::RootLike, ProcessorError> {
            *parser_flag.lock().unwrap() = true;
            parse(input)
                .map(postcss::ast::nodes::RootLike::Root)
                .map_err(ProcessorError::from)
        },
    );

    let processor = Processor::new();
    let mut lazy = processor
        .process_with_options(css, options)
        .expect("process with overridden parser");
    let _ = lazy.css().expect("css");

    assert!(*parser_called.lock().unwrap());
    assert!(!*syntax_called.lock().unwrap());
}

#[test]
fn stringifier_option_overrides_syntax_stringifier() {
    let css = "a { color: black; }";
    let syntax_called = Arc::new(Mutex::new(false));
    let stringifier_called = Arc::new(Mutex::new(false));
    let syntax_flag = Arc::clone(&syntax_called);
    let stringifier_flag = Arc::clone(&stringifier_called);
    let syntax = postcss::SyntaxOptions::new().stringifier(
        move |root_like: &postcss::ast::nodes::RootLike,
              builder: &mut dyn FnMut(
            &str,
            Option<&postcss::ast::NodeRef>,
            Option<&'static str>,
        )| {
            *syntax_flag.lock().unwrap() = true;
            let root = root_like
                .as_root()
                .expect("syntax stringifier should receive root");
            postcss::stringifier::stringify_with_builder(root.raw(), builder);
        },
    );
    let options = ProcessOptions::new().syntax(syntax).stringifier(
        move |root_like: &postcss::ast::nodes::RootLike,
              builder: &mut dyn FnMut(
            &str,
            Option<&postcss::ast::NodeRef>,
            Option<&'static str>,
        )| {
            *stringifier_flag.lock().unwrap() = true;
            let root = root_like
                .as_root()
                .expect("custom stringifier should receive root");
            postcss::stringifier::stringify_with_builder(root.raw(), builder);
        },
    );

    let processor = Processor::new();
    let mut lazy = processor
        .process_with_options(css, options)
        .expect("process with overridden stringifier");
    let _ = lazy.css().expect("css");

    assert!(*stringifier_called.lock().unwrap());
    assert!(!*syntax_called.lock().unwrap());
}

#[test]
fn root_to_result_with_options_respects_custom_stringifier() {
    let css = "a { color: black; }";
    let root = parse(css).expect("parse css into root");
    let called = Arc::new(Mutex::new(false));
    let flag = Arc::clone(&called);
    let options = ProcessOptions::new().stringifier(
        move |node_like: &postcss::ast::nodes::RootLike,
              builder: &mut dyn FnMut(
            &str,
            Option<&postcss::ast::NodeRef>,
            Option<&'static str>,
        )| {
            *flag.lock().unwrap() = true;
            let root = node_like
                .as_root()
                .expect("custom stringifier should receive root");
            postcss::stringifier::stringify_with_builder(root.raw(), &mut |chunk: &str,
                                                                           node_ref: Option<
                &postcss::ast::NodeRef,
            >,
                                                                           kind: Option<
                &'static str,
            >| {
                let replaced = chunk.replace("black", "white");
                builder(&replaced, node_ref, kind);
            });
        },
    );

    let mut result = root
        .to_result_with_options(options)
        .expect("root to result with options");
    let css_output = result.css().to_string();

    assert!(css_output.contains("white"));
    assert!(*called.lock().unwrap());
}

#[test]
fn css_syntax_error_renders_source() {
    let mut opts = InputOptions::default();
    opts.from = Some("test.css".into());
    let input = Input::new("a {\n  color: red;\n}\n", opts).expect("input");
    let error = input.error("Unknown word", Position::new(2, 3, 0), None);
    let code = error.show_source_code(Some(false));
    assert!(code.contains(" 2 |   color: red;"));
    assert!(code.contains('^'));
    let display = error.to_string();
    assert!(display.contains("CssSyntaxError: test.css:2:3: Unknown word"));
}

#[test]
fn from_json_rehydrates_root_from_js() {
    let data = run_js_root_json("a { color: black; }");
    let output = from_json(&data.json).expect("rehydrate root");
    let node = output.into_node().expect("root node");
    let mut css = String::new();
    postcss::stringifier::stringify_with_builder(&node, |text, _, _| css.push_str(text));
    assert_eq!(css, data.rehydrated);
}

#[test]
fn from_json_rehydrates_declaration_from_js() {
    let data = run_js_decl_json("a { color: red !important }");
    let output = from_json(&data.json).expect("rehydrate decl");
    let node = output.into_node().expect("decl node");
    let declaration = as_declaration(&node).expect("declaration");
    assert_eq!(declaration.prop(), "color");
    assert_eq!(declaration.value(), "red");
    assert!(declaration.important());
    let mut css = String::new();
    postcss::stringifier::stringify_with_builder(&node, |text, _, _| css.push_str(text));
    assert_eq!(css, data.rehydrated);
}

#[test]
fn to_json_roundtrips_through_from_json() {
    let css = "a { color: black }";
    let root = parse(css).expect("parse");
    let json = to_json(root.raw());

    let hydrated = from_json(&json).expect("rehydrate");
    let node = hydrated.into_node().expect("node");
    let mut output = String::new();
    postcss::stringifier::stringify_with_builder(&node, |text, _, _| output.push_str(text));

    assert_eq!(output, stringify(&root));
}

#[test]
fn to_json_matches_js_root_serialization() {
    let css = "a { color: black; }";
    let js = run_js_root_json(css);
    let root = parse(css).expect("parse");
    let mut json = to_json(root.raw());
    let mut js_json = js.json.clone();
    normalize_json(&mut json);
    normalize_json(&mut js_json);
    assert_eq!(json, js_json);
}

#[test]
fn vendor_prefix_and_unprefixed_behaviour() {
    assert_eq!(postcss::vendor::prefix("color"), "");
    assert_eq!(postcss::vendor::prefix("-webkit-border-radius"), "-webkit-");
    assert_eq!(postcss::vendor::prefix("-moz-transition"), "-moz-");
    assert_eq!(postcss::vendor::prefix("--custom-prop"), "");
    assert_eq!(postcss::vendor::prefix("-o"), "");

    assert_eq!(postcss::vendor::unprefixed("color"), "color");
    assert_eq!(
        postcss::vendor::unprefixed("-webkit-border-radius"),
        "border-radius"
    );
    assert_eq!(postcss::vendor::unprefixed("-moz-transition"), "transition");
    assert_eq!(
        postcss::vendor::unprefixed("--custom-prop"),
        "--custom-prop"
    );
    assert_eq!(postcss::vendor::unprefixed("-o"), "-o");
}

#[test]
fn lazy_result_stringify_sync_and_async_align() {
    let processor = Processor::new();
    let css = "a { color: black }";

    let mut lazy_for_string = processor.process(css).expect("lazy result");
    let css_string = lazy_for_string.stringify().expect("stringify");

    let mut lazy_for_alias = processor.process(css).expect("lazy result alias");
    let alias_string = lazy_for_alias.to_css_string().expect("to_css_string alias");
    assert_eq!(css_string, alias_string);

    let mut sync_result = processor
        .process(css)
        .expect("lazy for sync")
        .sync()
        .expect("sync result");
    let sync_css = sync_result.css().to_string();
    assert_eq!(css_string, sync_css);

    let mut async_result =
        block_on(processor.process(css).expect("lazy for async").r#async()).expect("async result");
    let async_css = async_result.css().to_string();
    assert_eq!(css_string, async_css);
}

#[test]
fn node_clone_helpers_mirror_postcss_behaviour() {
    let css = "a {\n  color: black;\n}\n";
    let root = parse(css).expect("parse");
    let rule_node = root.nodes().into_iter().next().expect("rule node present");
    let rule = as_rule(&rule_node).expect("rule wrapper");
    let decl_node = rule
        .nodes()
        .into_iter()
        .next()
        .expect("declaration node present");
    let decl = as_declaration(&decl_node).expect("declaration wrapper");

    let detached = decl.clone();
    assert_eq!(detached.prop(), "color");
    assert!(detached.parent().is_none());

    let cloned_rule = rule.clone();
    assert_eq!(cloned_rule.selector(), rule.selector());
    assert!(cloned_rule.parent().is_none());

    let rule_clone_inserted = rule.clone_after().expect("clone after root");
    let root_nodes = root.nodes();
    assert!(root_nodes.len() >= 2);
    assert!(Rc::ptr_eq(
        &root_nodes[root_nodes.len() - 1],
        &rule_clone_inserted.to_node()
    ));

    let before = decl.clone_before().expect("clone before");
    let after = decl.clone_after().expect("clone after");
    let rule_nodes = rule.nodes();
    assert!(rule_nodes.len() >= 3);
    assert!(Rc::ptr_eq(&rule_nodes[0], &before.to_node()));
    assert!(Rc::ptr_eq(
        &rule_nodes[rule_nodes.len() - 1],
        &after.to_node()
    ));

    let renamed = decl.clone_with(|clone| clone.set_prop("background"));
    assert_eq!(renamed.prop(), "background");
    assert!(renamed.parent().is_none());

    let inserted = decl
        .clone_before_with(|clone| clone.set_prop("border"))
        .expect("clone before with overrides");
    let rule_nodes = rule.nodes();
    assert!(rule_nodes
        .iter()
        .any(|node| Rc::ptr_eq(node, &inserted.to_node())));

    let at = at_rule_ctor("media");
    at.set_name("supports");
    at.set_params("(display: grid)");
    assert_eq!(at.name(), "supports");
    assert_eq!(at.params(), "(display: grid)");

    let document = document_ctor();
    assert!(document.mode().is_none());
    document.set_mode(Some("css".to_string()));
    assert_eq!(document.mode().as_deref(), Some("css"));
    document.set_mode::<String>(None);
    assert!(document.mode().is_none());

    let comment = comment_ctor("note");
    rule.append(comment.to_node());
    let rule_nodes = rule.nodes();
    let comment_node = rule_nodes.last().expect("comment appended to rule").clone();
    let comment_wrapper = as_comment(&comment_node).expect("comment wrapper");
    let appended_clone = comment_wrapper
        .clone_after()
        .expect("clone after comment with parent");
    let rule_nodes = rule.nodes();
    assert!(rule_nodes
        .iter()
        .any(|node| Rc::ptr_eq(node, &appended_clone.to_node())));
}

#[test]
fn node_position_and_error_helpers_match_postcss() {
    let css = "a {\n  color: black;\n}\n";
    let root = parse(css).expect("parse");
    let rule_node = root.nodes().into_iter().next().expect("rule node present");
    let rule = as_rule(&rule_node).expect("rule wrapper");
    let decl_node = rule
        .nodes()
        .into_iter()
        .next()
        .expect("declaration node present");
    let decl = as_declaration(&decl_node).expect("declaration wrapper");

    let position = decl
        .position_by(PositionByOptions {
            word: Some("color".into()),
            ..Default::default()
        })
        .expect("position lookup");
    assert_eq!(position.line, 2);
    assert_eq!(position.column, 3);

    let (start, end) = decl
        .range_by(RangeByOptions {
            word: Some("color".into()),
            ..Default::default()
        })
        .expect("range lookup");
    assert_eq!(start.line, 2);
    assert_eq!(start.column, 3);
    assert_eq!(end.line, 2);
    assert_eq!(end.column, 3 + "color".encode_utf16().count() as u32);

    let raw_semicolon = decl.raw_value("semicolon", Some("semicolon"));
    assert!(raw_semicolon.as_flag().is_some());

    let stringifier = (|node: &RootLike,
                        builder: &mut dyn FnMut(
        &str,
        Option<&postcss::ast::NodeRef>,
        Option<&'static str>,
    )| {
        let node_ref = node.to_node();
        postcss::stringifier::stringify_with_builder(&node_ref, builder);
    })
    .into_stringifier();

    let mut result = PostcssResult::new(
        RootLike::from(root.clone()),
        postcss::ProcessorMetadata::new("8.4.31"),
        postcss::ResultOptions::default(),
        stringifier,
    );
    let warning = decl.warn(&mut result, "check", WarningOptions::default());
    assert_eq!(warning.text, "check");
    assert_eq!(result.warnings().len(), 1);

    let error = decl.error_with_opts(
        "issue",
        RangeByOptions {
            word: Some("color".into()),
            ..Default::default()
        },
    );
    assert!(error.to_string().contains("issue"));
}
