use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use postcss::source_map::{MapAnnotation, MapSetting, PrevMap};
use postcss::{ProcessOptions, Processor};

#[derive(Debug, serde::Deserialize)]
struct JsWarning {
    text: String,
}

#[derive(Debug, serde::Deserialize)]
struct JsOutput {
    css: String,
    map: Option<String>,
    warnings: Vec<JsWarning>,
}

fn run_js(css: &str, options: &ProcessOptions) -> JsOutput {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let js_path = manifest_dir.join("../../lib/postcss.js");
    let js_path = js_path
        .canonicalize()
        .expect("canonicalize postcss lib path");

    let require_path = serde_json::to_string(
        js_path
            .to_str()
            .expect("postcss.js path must be valid utf-8"),
    )
    .expect("serialize path");

    let mut options_obj = serde_json::Map::new();
    if let Some(from) = &options.from {
        options_obj.insert("from".to_string(), serde_json::Value::String(from.clone()));
    }
    if let Some(to) = &options.to {
        options_obj.insert("to".to_string(), serde_json::Value::String(to.clone()));
    }

    match &options.map {
        MapSetting::Auto => {}
        MapSetting::Disabled => {
            options_obj.insert("map".to_string(), serde_json::Value::Bool(false));
        }
        MapSetting::Enabled(map_opts) => {
            let mut map_obj = serde_json::Map::new();
            if let Some(inline) = map_opts.inline {
                map_obj.insert("inline".to_string(), serde_json::Value::Bool(inline));
            }
            match &map_opts.annotation {
                MapAnnotation::Default => {}
                MapAnnotation::Disabled => {
                    map_obj.insert("annotation".to_string(), serde_json::Value::Bool(false));
                }
                MapAnnotation::String(value) => {
                    map_obj.insert(
                        "annotation".to_string(),
                        serde_json::Value::String(value.clone()),
                    );
                }
                MapAnnotation::Callback(_) => {}
            }
            if let Some(sources_content) = map_opts.sources_content {
                map_obj.insert(
                    "sourcesContent".to_string(),
                    serde_json::Value::Bool(sources_content),
                );
            }
            if let Some(prev) = &map_opts.prev {
                match prev {
                    PrevMap::Text(text) => {
                        map_obj.insert("prev".to_string(), serde_json::Value::String(text.clone()));
                    }
                    PrevMap::Object(value) => {
                        map_obj.insert("prev".to_string(), value.clone());
                    }
                    PrevMap::Disabled => {}
                    PrevMap::Function(_) | PrevMap::SourceMap(_) => {}
                }
            }
            if map_opts.absolute {
                map_obj.insert("absolute".to_string(), serde_json::Value::Bool(true));
            }
            options_obj.insert("map".to_string(), serde_json::Value::Object(map_obj));
        }
    }

    let options_arg =
        serde_json::to_string(&serde_json::Value::Object(options_obj)).expect("serialize options");

    let script = format!(
        "const fs=require('fs');const postcss=require({require_path});const css=fs.readFileSync(0,'utf8');const options={options_arg};if(!Object.prototype.hasOwnProperty.call(options,'from')) options.from=undefined;postcss().process(css,options).then(result=>{{const map=result.map?result.map.toString():null;const warnings=result.warnings().map(w=>({{text:w.text}}));console.log(JSON.stringify({{css:result.css,map,warnings}}));}}).catch(err=>{{console.error(err);process.exit(1);}});"
    );

    let mut child = Command::new("node")
        .arg("-e")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn node");

    {
        let stdin = child.stdin.as_mut().expect("stdin available");
        stdin.write_all(css.as_bytes()).expect("write css to node");
    }

    let output = child.wait_with_output().expect("wait for node output");

    assert!(
        output.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    serde_json::from_slice(&output.stdout).expect("parse node output")
}

fn collect_warning_texts(warnings: &[postcss::Warning]) -> Vec<String> {
    warnings
        .iter()
        .map(|warning| warning.text.clone())
        .collect()
}

fn normalize_css(value: &str) -> String {
    value.chars().filter(|c| !c.is_whitespace()).collect()
}

#[test]
fn parity_matches_js_for_basic_css() {
    let css = "a { color: black }";
    let options = ProcessOptions::new();
    let js = run_js(css, &options);

    let processor = Processor::new();
    let mut lazy = processor
        .process_with_options(css, options)
        .expect("process");

    let rust_css = lazy.css().expect("css").to_string();
    let rust_warnings = {
        let warnings = lazy.warnings().expect("warnings");
        collect_warning_texts(warnings)
    };
    let rust_map = lazy.map().expect("map").map(|map| map.to_string());

    assert_eq!(
        normalize_css(&rust_css),
        normalize_css(&js.css),
        "Token stream differs from JS output"
    );
    let js_warnings: Vec<String> = js.warnings.into_iter().map(|w| w.text).collect();
    assert_eq!(rust_warnings, js_warnings, "warning output differs");
    assert_eq!(rust_map, js.map, "source map output differs");
}

#[test]
fn parity_matches_js_for_comments_and_whitespace() {
    let css = "/*test*/\n@media screen {\n  a, b { color: black }\n}\n";
    let options = ProcessOptions::new();
    let js = run_js(css, &options);

    let processor = Processor::new();
    let mut lazy = processor
        .process_with_options(css, options)
        .expect("process");

    let rust_css = lazy.css().expect("css").to_string();
    let rust_warnings = {
        let warnings = lazy.warnings().expect("warnings");
        collect_warning_texts(warnings)
    };
    let rust_map = lazy.map().expect("map").map(|map| map.to_string());

    assert_eq!(
        normalize_css(&rust_css),
        normalize_css(&js.css),
        "Token stream differs from JS output",
    );
    let js_warnings: Vec<String> = js.warnings.into_iter().map(|w| w.text).collect();
    assert_eq!(rust_warnings, js_warnings, "warning output differs");
    assert_eq!(rust_map, js.map, "source map output differs");
}
