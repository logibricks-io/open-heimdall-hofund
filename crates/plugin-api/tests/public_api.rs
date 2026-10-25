//! Reviewed list of the public surface.
//!
//! `cargo-public-api` needs rustdoc JSON. The pinned stable toolchain's
//! rustdoc JSON is still an unstable option, so this test walks the source
//! with `syn` and records function signatures, `#[non_exhaustive]`, and
//! derives. A pull request that changes any of those updates `public-api.txt`.

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
        fs::write(&path, &actual).expect("write snapshot");
    }
    let expected = fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        expected, actual,
        "public API drifted; review the diff and set UPDATE_PUBLIC_API=1 to refresh public-api.txt"
    );
}

fn surface(src: &Path) -> String {
    let lib = parse(&src.join("lib.rs"));
    let mut exported = BTreeSet::new();
    let mut lines = BTreeSet::new();
    for item in &lib.items {
        if is_cfg_test(item_attrs(item)) {
            continue;
        }
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
            syn::Item::Mod(item) if is_pub(&item.vis) => {
                lines.insert(format!("mod {}", item.ident));
            }
            _ => {}
        }
    }

    for entry in fs::read_dir(src).expect("src") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("");
        if stem == "lib" {
            continue;
        }
        let in_kernel = stem == "kernel";
        let file = parse(&path);
        collect_file(&file.items, in_kernel, &exported, &mut lines);
    }

    let mut text = lines.into_iter().collect::<Vec<_>>().join("\n");
    text.push('\n');
    text
}

fn collect_file(
    items: &[syn::Item],
    in_kernel: bool,
    exported: &BTreeSet<String>,
    lines: &mut BTreeSet<String>,
) {
    for item in items {
        if is_cfg_test(item_attrs(item)) {
            continue;
        }
        match item {
            syn::Item::Mod(module) if module.ident == "tests" => {}
            syn::Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    collect_file(nested, in_kernel, exported, lines);
                }
            }
            syn::Item::Struct(item) if visible(in_kernel, &item.vis, &item.ident, exported) => {
                let name = qualify(in_kernel, &item.ident.to_string());
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
            syn::Item::Enum(item) if visible(in_kernel, &item.vis, &item.ident, exported) => {
                let name = qualify(in_kernel, &item.ident.to_string());
                lines.insert(shape_line(&name, &item.attrs));
                for variant in &item.variants {
                    lines.insert(format!("{name}::{}", variant.ident));
                }
            }
            syn::Item::Trait(item) if visible(in_kernel, &item.vis, &item.ident, exported) => {
                let name = qualify(in_kernel, &item.ident.to_string());
                lines.insert(name.clone());
                for trait_item in &item.items {
                    match trait_item {
                        syn::TraitItem::Fn(function) => {
                            lines.insert(format!("{name}::{}", tokens(&function.sig)));
                        }
                        syn::TraitItem::Const(constant) => {
                            lines.insert(format!("{name}::{}", constant.ident));
                        }
                        syn::TraitItem::Type(associated) => {
                            lines.insert(format!("{name}::{}", associated.ident));
                        }
                        _ => {}
                    }
                }
            }
            syn::Item::Fn(item) if visible(in_kernel, &item.vis, &item.sig.ident, exported) => {
                lines.insert(qualify(in_kernel, &tokens(&item.sig)));
            }
            syn::Item::Const(item) if visible(in_kernel, &item.vis, &item.ident, exported) => {
                lines.insert(format!(
                    "const {}",
                    qualify(in_kernel, &item.ident.to_string())
                ));
            }
            syn::Item::Type(item) if visible(in_kernel, &item.vis, &item.ident, exported) => {
                lines.insert(qualify(in_kernel, &item.ident.to_string()));
            }
            syn::Item::Use(item) if in_kernel && is_pub(&item.vis) => {
                for name in use_names(&item.tree) {
                    lines.insert(qualify(in_kernel, &name));
                }
            }
            syn::Item::Macro(item) if item.mac.path.is_ident("kebab_id") => {
                if let Some(name) = first_type_ident(&item.mac.tokens.to_string())
                    && (exported.contains(&name) || in_kernel)
                {
                    let name = qualify(in_kernel, &name);
                    lines.insert(name.clone());
                    lines.insert(format!("{name}::from_static"));
                    lines.insert(format!("{name}::as_str"));
                }
            }
            syn::Item::Impl(item) => {
                if item.trait_.is_some() {
                    continue;
                }
                let Some(type_name) = type_ident(&item.self_ty) else {
                    continue;
                };
                if !exported.contains(&type_name) && !in_kernel {
                    continue;
                }
                let name = qualify(in_kernel, &type_name);
                for impl_item in &item.items {
                    match impl_item {
                        syn::ImplItem::Fn(function) if is_pub(&function.vis) => {
                            lines.insert(format!("{name}::{}", tokens(&function.sig)));
                        }
                        syn::ImplItem::Const(constant) if is_pub(&constant.vis) => {
                            lines.insert(format!("{name}::{}", constant.ident));
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

fn tokens(value: &impl ToTokens) -> String {
    value
        .to_token_stream()
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn visible(
    in_kernel: bool,
    vis: &syn::Visibility,
    ident: &syn::Ident,
    exported: &BTreeSet<String>,
) -> bool {
    if !is_pub(vis) {
        return false;
    }
    in_kernel || exported.contains(&ident.to_string())
}

fn qualify(in_kernel: bool, name: &str) -> String {
    if in_kernel {
        format!("kernel::{name}")
    } else {
        name.to_owned()
    }
}

fn is_pub(vis: &syn::Visibility) -> bool {
    matches!(vis, syn::Visibility::Public(_))
}

fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("cfg") {
            return false;
        }
        let syn::Meta::List(list) = &attr.meta else {
            return false;
        };
        list.tokens.to_string() == "test"
    })
}

fn item_attrs(item: &syn::Item) -> &[syn::Attribute] {
    match item {
        syn::Item::Mod(item) => &item.attrs,
        syn::Item::Struct(item) => &item.attrs,
        syn::Item::Enum(item) => &item.attrs,
        syn::Item::Trait(item) => &item.attrs,
        syn::Item::Fn(item) => &item.attrs,
        syn::Item::Const(item) => &item.attrs,
        syn::Item::Type(item) => &item.attrs,
        syn::Item::Use(item) => &item.attrs,
        syn::Item::Impl(item) => &item.attrs,
        syn::Item::Macro(item) => &item.attrs,
        _ => &[],
    }
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

fn first_type_ident(tokens: &str) -> Option<String> {
    let mut chars = tokens.chars().peekable();
    let mut in_string = false;
    let mut current = String::new();
    while let Some(character) = chars.next() {
        if in_string {
            if character == '\\' {
                chars.next();
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        if character == '"' {
            in_string = true;
            continue;
        }
        if character.is_ascii_alphanumeric() || character == '_' {
            current.push(character);
            continue;
        }
        if current
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase())
        {
            return Some(current);
        }
        current.clear();
    }
    None
}

fn parse(path: &Path) -> syn::File {
    let text =
        fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    syn::parse_file(&text).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}
