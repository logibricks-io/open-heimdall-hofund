//! Reviewed public surface and forbidden-type boundary.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use quote::ToTokens;

#[test]
fn public_surface_matches_the_snapshot() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let actual = surface(&root.join("src"));
    let path = root.join("public-api.txt");
    if std::env::var_os("UPDATE_PUBLIC_API").is_some() {
        fs::write(&path, &actual).expect("write public API snapshot");
    }
    let expected = fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        expected, actual,
        "public API drifted; review the diff and set UPDATE_PUBLIC_API=1 to refresh public-api.txt"
    );
}

#[test]
fn public_api_names_no_plugin_router_or_provider_type() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let forbidden = [
        "ProviderError",
        "ProviderErrorKind",
        "CooldownPolicy",
        "CooldownCause",
        "PluginError",
        "hofund_plugin_api",
        "hofund_router",
        "hofund_provider",
        "Instant",
        "Duration",
        "MemberId",
    ];
    let mut texts = vec![fs::read_to_string(root.join("public-api.txt")).expect("public API")];
    for entry in fs::read_dir(root.join("src")).expect("src") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|extension| extension == "rs") {
            texts.push(fs::read_to_string(path).expect("source"));
        }
    }
    for text in texts {
        for name in forbidden {
            assert!(!text.contains(name), "public boundary contains {name}");
        }
    }

    let manifest = fs::read_to_string(root.join("Cargo.toml")).expect("manifest");
    let mut normal = BTreeSet::new();
    let mut in_normal = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_normal = line == "[dependencies]";
            continue;
        }
        if in_normal && !line.is_empty() {
            normal.insert(
                line.split('=')
                    .next()
                    .expect("dependency name")
                    .trim()
                    .to_owned(),
            );
        }
    }
    assert_eq!(
        normal,
        ["serde", "serde_json", "thiserror"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
}

fn surface(src: &Path) -> String {
    let lib = parse(&src.join("lib.rs"));
    let mut exported = BTreeSet::new();
    let mut lines = BTreeSet::new();
    for item in &lib.items {
        match item {
            syn::Item::Use(item) if is_pub(&item.vis) => {
                for name in use_names(&item.tree) {
                    exported.insert(name.clone());
                    lines.insert(name);
                }
            }
            syn::Item::Const(item) if is_pub(&item.vis) => {
                let name = item.ident.to_string();
                exported.insert(name.clone());
                lines.insert(format!("const {name}"));
            }
            _ => {}
        }
    }

    for entry in fs::read_dir(src).expect("src") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        if path.file_stem().and_then(|stem| stem.to_str()) == Some("lib") {
            continue;
        }
        let file = parse(&path);
        collect_file(&file.items, &exported, &mut lines);
    }

    let mut text = lines.into_iter().collect::<Vec<_>>().join("\n");
    text.push('\n');
    text
}

fn collect_file(items: &[syn::Item], exported: &BTreeSet<String>, lines: &mut BTreeSet<String>) {
    for item in items {
        match item {
            syn::Item::Struct(item) if visible(&item.vis, &item.ident, exported) => {
                let name = item.ident.to_string();
                lines.insert(shape_line(&name, &item.attrs));
                if let syn::Fields::Named(fields) = &item.fields {
                    for field in &fields.named {
                        if is_pub(&field.vis)
                            && let Some(ident) = &field.ident
                        {
                            lines.insert(format!("{name}.{ident}"));
                        }
                    }
                }
            }
            syn::Item::Enum(item) if visible(&item.vis, &item.ident, exported) => {
                let name = item.ident.to_string();
                lines.insert(shape_line(&name, &item.attrs));
                for variant in &item.variants {
                    let mut line = format!("{name}::{}", variant.ident);
                    if has_serde_attr(&variant.attrs, "other") {
                        line.push_str(" #[serde(other)]");
                    }
                    lines.insert(line);
                }
            }
            syn::Item::Trait(item) if visible(&item.vis, &item.ident, exported) => {
                let name = item.ident.to_string();
                lines.insert(name.clone());
                for trait_item in &item.items {
                    if let syn::TraitItem::Fn(function) = trait_item {
                        lines.insert(format!("{name}::{}", tokens(&function.sig)));
                    }
                }
            }
            syn::Item::Fn(item) if visible(&item.vis, &item.sig.ident, exported) => {
                lines.insert(tokens(&item.sig));
            }
            syn::Item::Const(item) if visible(&item.vis, &item.ident, exported) => {
                lines.insert(format!("const {}", item.ident));
            }
            syn::Item::Impl(item) => {
                if item.trait_.is_some() {
                    continue;
                }
                let Some(type_name) = type_ident(&item.self_ty) else {
                    continue;
                };
                if !exported.contains("*") && !exported.contains(&type_name) {
                    continue;
                }
                for impl_item in &item.items {
                    match impl_item {
                        syn::ImplItem::Fn(function) if is_pub(&function.vis) => {
                            lines.insert(format!("{type_name}::{}", tokens(&function.sig)));
                        }
                        syn::ImplItem::Const(constant) if is_pub(&constant.vis) => {
                            lines.insert(format!("{type_name}::{}", constant.ident));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

fn shape_line(name: &str, attrs: &[syn::Attribute]) -> String {
    let mut line = name.to_owned();
    if attrs
        .iter()
        .any(|attr| attr.path().is_ident("non_exhaustive"))
    {
        line.push_str(" #[non_exhaustive]");
    }
    if let Some(derives) = derive_list(attrs) {
        line.push(' ');
        line.push_str(&derives);
    }
    for attr in attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        let text = tokens(attr);
        for needle in ["deny_unknown_fields", "tag", "content", "rename_all"] {
            if text.contains(needle) {
                line.push(' ');
                line.push_str(&text);
                break;
            }
        }
    }
    line
}

fn derive_list(attrs: &[syn::Attribute]) -> Option<String> {
    for attr in attrs {
        if !attr.path().is_ident("derive") {
            continue;
        }
        let syn::Meta::List(list) = &attr.meta else {
            continue;
        };
        let Ok(nested) = list.parse_args_with(
            syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
        ) else {
            continue;
        };
        let names = nested
            .iter()
            .filter_map(|path| {
                path.segments
                    .last()
                    .map(|segment| segment.ident.to_string())
            })
            .collect::<Vec<_>>()
            .join(", ");
        if !names.is_empty() {
            return Some(format!("derives({names})"));
        }
    }
    None
}

fn has_serde_attr(attrs: &[syn::Attribute], name: &str) -> bool {
    attrs
        .iter()
        .any(|attr| attr.path().is_ident("serde") && tokens(attr).contains(name))
}

fn tokens(value: &impl ToTokens) -> String {
    value
        .to_token_stream()
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn visible(vis: &syn::Visibility, ident: &syn::Ident, exported: &BTreeSet<String>) -> bool {
    is_pub(vis) && (exported.contains("*") || exported.contains(&ident.to_string()))
}

fn is_pub(vis: &syn::Visibility) -> bool {
    matches!(vis, syn::Visibility::Public(_))
}

fn use_names(tree: &syn::UseTree) -> Vec<String> {
    let mut names = Vec::new();
    walk_use(tree, &mut names);
    names
}

fn walk_use(tree: &syn::UseTree, names: &mut Vec<String>) {
    match tree {
        syn::UseTree::Name(name) => names.push(name.ident.to_string()),
        syn::UseTree::Rename(name) => names.push(name.rename.to_string()),
        syn::UseTree::Path(path) => walk_use(&path.tree, names),
        syn::UseTree::Group(group) => {
            for item in &group.items {
                walk_use(item, names);
            }
        }
        syn::UseTree::Glob(_) => names.push("*".to_owned()),
    }
}

fn type_ident(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(path) => path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string()),
        syn::Type::Group(group) => type_ident(&group.elem),
        _ => None,
    }
}

fn parse(path: &Path) -> syn::File {
    let text = fs::read_to_string(path).expect("read Rust source");
    syn::parse_file(&text).expect("parse Rust source")
}
