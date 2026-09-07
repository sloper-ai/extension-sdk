use std::{
    error::Error as StdError,
    fmt::Debug,
};

use serde_json::Value;
use sloper_extension_host::{
    ComponentError,
    assemble_parts,
    check_component,
    extract_manifest,
    extract_parts,
    stamp_manifest,
    validate_component,
};
use wit_component::{
    ComponentEncoder,
    StringEncoding,
    dummy_module,
    embed_component_metadata,
};
use wit_parser::{
    LiftLowerAbi,
    ManglingAndAbi,
    Resolve,
    UnresolvedPackageGroup,
    WorldId,
};

const HEADER: &[u8] = b"\0asm\x0d\0\x01\0";
const CORE_HEADER: &[u8] = b"\0asm\x01\0\0\0";
const MANIFEST: &[u8] = br#"{"name":"acme.demo","version":"1.0.0","description":"Demo","actions":{"run":{}}}"#;
const EXTENSION: &[u8] =
    br#"{"kind":"extension","name":"acme.demo","version":"1.0.0","description":"Demo","actions":["run"]}"#;
const ACTION: &[u8] = br#"{"kind":"action","name":"run"}"#;
const PACKAGES: &[(&str, &str)] = sloper_extension_spec::WIT_PACKAGES;

fn append_section(bytes: &[u8], id: u8, contents: &[u8]) -> Vec<u8> {
    let mut result = bytes.to_vec();
    result.push(id);
    leb(contents.len(), &mut result);
    result.extend_from_slice(contents);
    result
}

fn custom(name: &[u8], contents: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    leb(name.len(), &mut result);
    result.extend_from_slice(name);
    result.extend_from_slice(contents);
    result
}

fn leb(mut value: usize, result: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f).to_le_bytes()[0];
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        result.push(byte);
        if value == 0 {
            break;
        }
    }
}

#[track_caller]
fn assert_code<T: Debug>(result: Result<T, ComponentError>, code: &str) {
    let error = result.expect_err("the deliberately invalid component must be rejected");
    assert_eq!(error.findings()[0].code, code, "findings={:?}", error.findings());
}

fn rewrite_wit(text: &str, replacements: &[(&str, &str)]) -> String {
    replacements.iter().fold(
        text.lines()
            .filter(|line| !line.trim_start().starts_with("@since(") && !line.trim_start().starts_with("@deprecated("))
            .collect::<Vec<_>>()
            .join("\n"),
        |text, (from, to)| text.replace(from, to),
    )
}

fn compiled_component(replacements: &[(&str, &str)], fragments: &[&[u8]]) -> Vec<u8> {
    let packages = PACKAGES
        .iter()
        .map(|(name, text)| {
            UnresolvedPackageGroup::parse(name, &rewrite_wit(text, replacements)).expect("test dependency WIT parses")
        })
        .collect();
    let root = UnresolvedPackageGroup::parse(
        "extension.wit",
        &rewrite_wit(sloper_extension_spec::WIT_WORLD, replacements),
    )
    .expect("test world WIT parses");
    let mut resolve = Resolve::default();
    let package = resolve
        .push_groups(root, packages)
        .expect("test WIT dependencies resolve");
    let world = resolve
        .select_world(&[package], Some("extension"))
        .expect("test extension world exists");
    compile_world(&resolve, world, fragments)
}

fn compile_world(resolve: &Resolve, world: WorldId, fragments: &[&[u8]]) -> Vec<u8> {
    let mut module = dummy_module(resolve, world, ManglingAndAbi::Legacy(LiftLowerAbi::AsyncCallback));
    for fragment in fragments {
        module = append_section(&module, 0, &custom(b"sloper:parts", fragment));
    }
    embed_component_metadata(&mut module, resolve, world, StringEncoding::UTF8)
        .expect("metadata describes the generated core module");
    ComponentEncoder::default()
        .module(&module)
        .expect("core module metadata is valid")
        .validate(true)
        .encode()
        .expect("test executable component encodes")
}

fn trapping_component() -> Vec<u8> {
    let component = compiled_component(&[], &[]);
    let mut depth = 0;
    let mut modules = 0;
    for payload in wasmparser::Parser::new(0).parse_all(&component) {
        match payload.expect("generated component parses") {
            wasmparser::Payload::ModuleSection {
                ..
            } => {
                if depth == 0 {
                    modules += 1;
                }
                depth += 1;
            },
            wasmparser::Payload::ComponentSection {
                ..
            } => depth += 1,
            wasmparser::Payload::End(_) if depth > 0 => depth -= 1,
            _ => {},
        }
    }
    let trap = wat::parse_str("(module (func $start unreachable) (start $start))")
        .expect("trapping core module has valid syntax");
    let component = append_section(&component, 1, &trap);
    let mut instance = vec![1, 0]; // One core instance, using module instantiation.
    leb(modules, &mut instance);
    instance.push(0); // The trapping module has no imports.
    let component = append_section(&component, 2, &instance);
    stamp_manifest(&component, MANIFEST).expect("non-SDK component stamps")
}

#[test]
fn extraction_preserves_embedded_bytes_and_ignores_nested_manifests() {
    let authored = [b" \n".as_slice(), MANIFEST, b"\n".as_slice()].concat();
    let nested = append_section(CORE_HEADER, 0, &custom(b"sloper:manifest", b"nested"));
    let component = append_section(HEADER, 1, &nested);
    assert_code(extract_manifest(&component), "MANIFEST_ABSENT");
    let stamped = stamp_manifest(&component, &authored).expect("valid document stamps");
    assert_eq!(extract_manifest(&stamped).expect("top-level document exists"), authored);
    assert_eq!(&stamped[..component.len()], component);
    assert_code(stamp_manifest(&stamped, &authored), "EXTENSION_ALREADY_STAMPED");
}

#[test]
fn extraction_rejects_duplicate_manifest_even_when_bytes_match() {
    let section = custom(b"sloper:manifest", MANIFEST);
    let component = append_section(&append_section(HEADER, 0, &section), 0, &section);
    assert_code(extract_manifest(&component), "MANIFEST_DUPLICATED");
}

#[test]
fn extraction_rejects_bad_headers_truncated_and_overflowing_lengths() {
    for component in [
        b"".as_slice(),
        b"\0asm\x0d\0\x01".as_slice(),
        CORE_HEADER,
        b"\0asm\x0d\0\x01\x01".as_slice(),
        b"\0asm\x0d\0\x01\0\0".as_slice(),
        b"\0asm\x0d\0\x01\0\0\x80".as_slice(),
        b"\0asm\x0d\0\x01\0\0\x80\x80\x80\x80\x10".as_slice(),
        b"\0asm\x0d\0\x01\0\0\xff\xff\xff\xff\xff".as_slice(),
        b"\0asm\x0d\0\x01\0\0\x02\0".as_slice(),
        b"\0asm\x0d\0\x01\0\0\0".as_slice(),
    ] {
        assert_code(extract_manifest(component), "MANIFEST_MALFORMED");
    }
    for payload in [
        b"\x80".as_slice(),
        b"\x80\x80\x80\x80\x10".as_slice(),
        b"\x02x".as_slice(),
    ] {
        let component = append_section(
            &append_section(HEADER, 0, payload),
            0,
            &custom(b"sloper:manifest", MANIFEST),
        );
        assert_code(extract_manifest(&component), "MANIFEST_MALFORMED");
    }
}

#[test]
fn extraction_accepts_legal_nonminimal_leb_and_defers_component_version() {
    let mut component = HEADER.to_vec();
    component.extend_from_slice(&[0, 0x82, 0x80, 0x80, 0x80, 0, 1, b'x']);
    let component = append_section(&component, 0, &custom(b"sloper:manifest", MANIFEST));
    assert_eq!(
        extract_manifest(&component).expect("nonminimal u32 LEB is legal"),
        MANIFEST
    );
    let mut future = component;
    future[4] = 255;
    assert_eq!(
        extract_manifest(&future).expect("envelope does not own parser version"),
        MANIFEST
    );
    assert_code(validate_component(&future), "MANIFEST_WORLD_INVALID");
}

#[test]
fn extraction_keeps_utf8_cause_and_document_parser_rejects_invalid_payloads() {
    let bad_name = append_section(HEADER, 0, &custom(&[255], b""));
    let error = extract_manifest(&bad_name).expect_err("invalid name encoding must fail");
    assert_eq!(error.findings()[0].code, "MANIFEST_MALFORMED");
    assert!(error.source().is_some(), "UTF-8 decoding cause is retained");
    for payload in [b"\xff".as_slice(), br#"{"actions":{},"actions":{}}"#.as_slice()] {
        let component = append_section(HEADER, 0, &custom(b"sloper:manifest", payload));
        assert_code(validate_component(&component), "MANIFEST_MALFORMED");
    }
}

#[test]
fn extraction_enforces_manifest_and_component_inclusive_size_bounds() {
    let exact = vec![b' '; 256 * 1024];
    let component = append_section(HEADER, 0, &custom(b"sloper:manifest", &exact));
    assert_eq!(
        extract_manifest(&component).expect("exact byte bound is accepted"),
        exact
    );
    let excess = append_section(HEADER, 0, &custom(b"sloper:manifest", &vec![b' '; 256 * 1024 + 1]));
    assert_code(extract_manifest(&excess), "MANIFEST_TOO_LARGE");
    let mut oversized = vec![0; 64 * 1024 * 1024 + 1];
    oversized[..8].copy_from_slice(HEADER);
    assert_code(extract_manifest(&oversized), "MANIFEST_TOO_LARGE");
}

#[test]
fn parts_are_extracted_from_core_modules_inside_nested_components() {
    let core = append_section(CORE_HEADER, 0, &custom(b"sloper:parts", EXTENSION));
    let core = append_section(&core, 0, &custom(b"sloper:parts", ACTION));
    let nested = append_section(HEADER, 1, &core);
    let nested = append_section(&nested, 0, &custom(b"sloper:parts", b"ignored"));
    let outer = append_section(HEADER, 4, &nested);
    assert_eq!(
        extract_parts(&outer).expect("nested core declarations exist"),
        [EXTENSION, ACTION]
    );
    assert_eq!(
        extract_parts(&append_section(HEADER, 0, &custom(b"sloper:parts", EXTENSION)))
            .expect("component-level metadata is ignored"),
        Vec::<Vec<u8>>::new()
    );
    assert_code(extract_parts(CORE_HEADER), "EXTENSION_PARTS_INVALID");
    assert_code(
        extract_parts(&append_section(HEADER, 1, HEADER)),
        "EXTENSION_PARTS_INVALID",
    );
    assert_code(
        extract_parts(&append_section(HEADER, 4, CORE_HEADER)),
        "EXTENSION_PARTS_INVALID",
    );
}

#[test]
fn parts_reject_excessive_nesting_size_and_count() {
    let mut nested = append_section(HEADER, 1, CORE_HEADER);
    for _ in 0..32 {
        nested = append_section(HEADER, 4, &nested);
    }
    assert_code(extract_parts(&nested), "EXTENSION_PARTS_INVALID");
    let core = append_section(CORE_HEADER, 0, &custom(b"sloper:parts", &vec![b' '; 256 * 1024 + 1]));
    assert_code(
        extract_parts(&append_section(HEADER, 1, &core)),
        "EXTENSION_PARTS_INVALID",
    );
    let mut core = CORE_HEADER.to_vec();
    for _ in 0..106 {
        core = append_section(&core, 0, &custom(b"sloper:parts", b"{}"));
    }
    assert_code(
        extract_parts(&append_section(HEADER, 1, &core)),
        "EXTENSION_PARTS_INVALID",
    );
}

#[test]
fn assembly_is_deterministic_and_preserves_schema_declaration_order() {
    let extension =
        br#"{"kind":"extension","name":"acme.demo","version":"1.0.0","description":"Demo","actions":["zeta","alpha"]}"#;
    let alpha = br#"{"kind":"action","name":"alpha","parameters":{"type":"object","properties":{"z":{"type":"string"},"a":{"type":"string"}},"required":["z","a"]}}"#;
    let zeta = br#"{"kind":"action","name":"zeta"}"#;
    let expected = assemble_parts(&[extension, zeta, alpha]).expect("all named fragments resolve");
    assert_eq!(
        expected,
        assemble_parts(&[alpha, extension, zeta]).expect("fragment order has no effect")
    );
    let document: Value = serde_json::from_slice(&expected).expect("assembled JSON is valid");
    assert_eq!(
        document["actions"]
            .as_object()
            .expect("actions object")
            .keys()
            .collect::<Vec<_>>(),
        ["alpha", "zeta"]
    );
    assert_eq!(
        document["actions"]["alpha"]["parameters"]["properties"]
            .as_object()
            .expect("properties object")
            .keys()
            .collect::<Vec<_>>(),
        ["z", "a"]
    );
    assert_eq!(
        document["actions"]["alpha"]["parameters"]["required"],
        serde_json::json!(["z", "a"])
    );
}

#[test]
fn assembly_supports_linker_concatenated_json_and_rejects_duplicate_json_keys() {
    let fragments = [EXTENSION, b"\n", ACTION].concat();
    assert_eq!(
        assemble_parts(&[&fragments]).expect("linker concatenates declaration data"),
        assemble_parts(&[EXTENSION, ACTION]).expect("two fragments assemble")
    );
    for invalid in [
        b"\xff".as_slice(),
        br#"{"kind":"action","name":"run","name":"other"}"#.as_slice(),
        b"{} trailing".as_slice(),
    ] {
        assert_code(assemble_parts(&[EXTENSION, invalid]), "EXTENSION_PARTS_INVALID");
    }
}

#[test]
fn assembly_checks_both_action_list_directions_and_uniqueness() {
    assert_code(assemble_parts(&[ACTION]), "EXTENSION_PARTS_ABSENT");
    assert_code(
        assemble_parts(&[EXTENSION, EXTENSION, ACTION]),
        "EXTENSION_PARTS_DUPLICATED",
    );
    assert_code(
        assemble_parts(&[EXTENSION, ACTION, ACTION]),
        "EXTENSION_PARTS_DUPLICATED",
    );
    assert_code(assemble_parts(&[EXTENSION]), "EXTENSION_ACTION_LIST_INVALID");
    for action_list in ["[]", "[\"run\",\"missing\"]", "[\"run\",\"run\"]", "[1]", "\"run\""] {
        let extension = String::from_utf8(EXTENSION.to_vec())
            .expect("fixture is UTF-8")
            .replace("[\"run\"]", action_list);
        assert_code(
            assemble_parts(&[extension.as_bytes(), ACTION]),
            "EXTENSION_ACTION_LIST_INVALID",
        );
    }
}

#[test]
fn assembly_derives_capability_union_and_preserves_explicit_invalid_declarations() {
    let action = br#"{"kind":"action","name":"run","mode":"background","resources":{"items":["write"]}}"#;
    let resource = br#"{"kind":"resource","name":"items","key":"id","schema":{"type":"object","properties":{"id":{"type":"string","maxLength":512}},"required":["id"]}}"#;
    let document: Value = serde_json::from_slice(
        &assemble_parts(&[EXTENSION, action, resource]).expect("writer derives write capability"),
    )
    .expect("manifest is JSON");
    assert_eq!(
        document["resources"]["items"]["capabilities"],
        serde_json::json!(["write"])
    );
    let invalid = String::from_utf8(resource.to_vec())
        .expect("fixture is UTF-8")
        .replace("\"key\":\"id\"", "\"key\":\"id\",\"capabilities\":[\"read\"]");
    assert_code(
        assemble_parts(&[EXTENSION, action, invalid.as_bytes()]),
        "MANIFEST_INVALID_VALUE",
    );
    let shadowed = String::from_utf8(EXTENSION.to_vec())
        .expect("fixture is UTF-8")
        .replace("\"actions\":", "\"resources\":{},\"actions\":");
    assert_code(
        assemble_parts(&[shadowed.as_bytes(), ACTION]),
        "EXTENSION_PARTS_INVALID",
    );
}

#[test]
fn complete_async_host_world_and_compatible_wasi_imports_are_validated_statically() {
    for wasi in ["0.2.0", "0.2.11", "0.2.12", "0.2.13", "0.2.999"] {
        let component = compiled_component(&[("0.2.12", wasi)], &[]);
        let stamped = stamp_manifest(&component, MANIFEST).expect("component stamps");
        validate_component(&stamped)
            .unwrap_or_else(|error| panic!("WASI {wasi} must validate: {error:?}; cause={:?}", error.source()));
    }
}

#[test]
fn stdlib_and_sloper_can_import_different_compatible_wasi_versions() {
    let mut packages = PACKAGES
        .iter()
        .map(|(name, text)| UnresolvedPackageGroup::parse(name, text).expect("canonical dependency WIT parses"))
        .collect::<Vec<_>>();
    packages.push(
        UnresolvedPackageGroup::parse("extension.wit", sloper_extension_spec::WIT_WORLD)
            .expect("canonical action world parses"),
    );
    packages.extend(
        PACKAGES
            .iter()
            .filter(|(name, _)| *name != "sloper-api.wit")
            .map(|(name, text)| {
                UnresolvedPackageGroup::parse(name, &rewrite_wit(text, &[("0.2.12", "0.2.0")]))
                    .expect("compatible dependency WIT parses")
            }),
    );
    let root = UnresolvedPackageGroup::parse(
        "combined.wit",
        "package sloper:fixture; world combined { include sloper:extension/extension@0.1.0; include \
         wasi:cli/imports@0.2.0; import wasi:http/types@0.2.0; import wasi:http/outgoing-handler@0.2.0; }",
    )
    .expect("combined fixture world parses");
    let mut resolve = Resolve::default();
    let package = resolve
        .push_groups(root, packages)
        .expect("both compatible WASI versions resolve");
    let world = resolve
        .select_world(&[package], Some("combined"))
        .expect("combined world exists");
    let component = compile_world(&resolve, world, &[]);
    let stamped = stamp_manifest(&component, MANIFEST).expect("component stamps");
    validate_component(&stamped).expect("mixed compatible versions satisfy the exact host world");
}

#[test]
fn compatible_import_versions_still_require_compatible_function_types() {
    let component = compiled_component(
        &[
            ("0.2.12", "0.2.0"),
            ("get-random-u64: func() -> u64", "get-random-u64: func() -> u32"),
        ],
        &[],
    );
    let stamped = stamp_manifest(&component, MANIFEST).expect("component stamps");
    assert_code(validate_component(&stamped), "MANIFEST_WORLD_INVALID");
}

#[test]
fn unsupported_import_generations_and_sloper_versions_are_rejected() {
    for replacement in [
        ("0.2.12", "0.3.0"),
        ("0.2.12", "0.1.0"),
        ("import sloper:api/log@0.1.0;", "import private: func();"),
        ("@0.1.0", "@0.1.1"),
    ] {
        let component = compiled_component(&[replacement], &[]);
        let stamped = stamp_manifest(&component, MANIFEST).expect("component stamps");
        assert_code(validate_component(&stamped), "MANIFEST_WORLD_INVALID");
    }
}

#[test]
fn valid_manifest_cannot_admit_missing_or_wrong_action_exports() {
    assert_code(
        validate_component(&stamp_manifest(HEADER, MANIFEST).expect("envelope stamps")),
        "MANIFEST_WORLD_INVALID",
    );
    for replacement in [
        ("run: async func(request: request)", "run: func(request: request)"),
        ("run: async func(request: request)", "run: async func(request: string)"),
        ("result<_, failure>;", "result<string, failure>;"),
        ("export action;", "export action; export surprise: func();"),
        (
            "run: async func(request: request)",
            "extra: func(); run: async func(request: request)",
        ),
    ] {
        let component = compiled_component(&[replacement], &[]);
        let stamped = stamp_manifest(&component, MANIFEST).expect("component stamps");
        assert_code(validate_component(&stamped), "MANIFEST_WORLD_INVALID");
    }
}

#[test]
fn check_reassembles_and_compares_exact_embedded_bytes() {
    let component = compiled_component(&[], &[EXTENSION, ACTION]);
    let assembled = assemble_parts(&[EXTENSION, ACTION]).expect("valid fragments assemble");
    let stamped = stamp_manifest(&component, &assembled).expect("component stamps");
    check_component(&stamped).expect("component and its exact assembly agree");
    let different = stamp_manifest(&component, MANIFEST).expect("semantically equivalent authored document stamps");
    assert_code(check_component(&different), "EXTENSION_MANIFEST_MISMATCH");
}

#[test]
fn static_publication_accepts_non_sdk_components_without_executing_their_start() {
    let component = trapping_component();
    validate_component(&component).expect("static validation does not execute the trapping start");
    assert_code(check_component(&component), "EXTENSION_PARTS_ABSENT");
}

#[cfg(feature = "runtime")]
#[tokio::test]
async fn runtime_admission_rejects_a_statically_valid_trapping_start() {
    let component = trapping_component();
    validate_component(&component).expect("component is statically valid");
    let engine = sloper_extension_host::Engine::new().expect("runtime engine initializes");
    assert!(engine.admit_component(&component).await.is_err());
}
