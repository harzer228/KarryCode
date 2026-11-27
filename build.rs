use proc_macro2::{TokenStream, TokenTree};
use std::{collections::BTreeMap, env, fs, path::Path};

fn collect(tokens: TokenStream, strings: &mut BTreeMap<String, bool>) {
    let mut tokens = tokens.into_iter().peekable();
    while let Some(token) = tokens.next() {
        match token {
            TokenTree::Ident(name)
                if matches!(name.to_string().as_str(), "tr" | "trf" | "localized_notice") =>
            {
                if name != "localized_notice" {
                    if !matches!(tokens.peek(), Some(TokenTree::Punct(mark)) if mark.as_char() == '!')
                    {
                        continue;
                    }
                    tokens.next();
                }
                let Some(TokenTree::Group(group)) = tokens.next() else {
                    continue;
                };
                let content = group.stream();
                let mut values = content.clone().into_iter();
                if let [
                    Some(TokenTree::Literal(_)),
                    Some(TokenTree::Punct(comma)),
                    Some(TokenTree::Literal(value)),
                ] = [values.next(), values.next(), values.next()]
                    && comma.as_char() == ','
                    && let Ok(text) = syn::parse_str::<syn::LitStr>(&value.to_string())
                {
                    *strings.entry(text.value()).or_default() |= name == "trf";
                }
                collect(content, strings);
            }
            TokenTree::Group(group) => collect(group.stream(), strings),
            _ => {}
        }
    }
}

fn sources(directory: &Path, strings: &mut BTreeMap<String, bool>) {
    for entry in fs::read_dir(directory).expect("reading KarryCode source directory") {
        let path = entry.expect("reading KarryCode source entry").path();
        if path.is_dir() {
            sources(&path, strings);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let source = fs::read_to_string(path).expect("reading source");
            collect(source.parse().expect("tokenizing source"), strings);
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=src");
    let mut strings = BTreeMap::new();
    sources(Path::new("src"), &mut strings);
    let code = format!("&{:?}", strings.into_iter().collect::<Vec<_>>());
    fs::write(
        Path::new(&env::var_os("OUT_DIR").unwrap()).join("messages.rs"),
        code,
    )
    .expect("writing the localization index");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/karrycode.rc");
        println!("cargo:rerun-if-changed=assets/karrycode.ico");
        embed_resource::compile("assets/karrycode.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("embedding the KarryCode application icon");
    }
}
