//! Source-level unsafe inventory. Does not expand macros or evaluate cfg.
//! Run: cargo run --locked --example unsafe_inventory -- src
use std::{
    env, fs,
    path::{Path, PathBuf},
};
use syn::{
    spanned::Spanned,
    visit::{self, Visit},
    Attribute,
};

#[derive(Default)]
struct Inventory {
    test: bool,
    conditions: Vec<usize>,
    sites: Vec<(usize, &'static str, bool, Vec<usize>)>,
}

impl Inventory {
    fn scoped(&mut self, attrs: &[Attribute], visit: impl FnOnce(&mut Self)) {
        let previous = self.test;
        let length = self.conditions.len();
        for attr in attrs {
            if attr.path().is_ident("test") || is_test_cfg(attr) {
                self.test = true;
            }
            if attr.path().is_ident("cfg") || attr.path().is_ident("cfg_attr") {
                self.conditions.push(attr.span().start().line);
            }
        }
        visit(self);
        self.test = previous;
        self.conditions.truncate(length);
    }

    fn record(&mut self, line: usize, kind: &'static str) {
        self.sites
            .push((line, kind, self.test, self.conditions.clone()));
    }
}

fn is_test_cfg(attr: &Attribute) -> bool {
    attr.path().is_ident("cfg")
        && attr
            .parse_args::<syn::Meta>()
            .is_ok_and(|meta| requires_test(&meta))
}

fn requires_test(meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::Path(path) => path.is_ident("test"),
        syn::Meta::List(list) => test_in_list(list),
        _ => false,
    }
}

fn test_in_list(list: &syn::MetaList) -> bool {
    let Ok(items) = list.parse_args_with(
        syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
    ) else {
        return false;
    };
    if list.path.is_ident("all") {
        items.iter().any(requires_test)
    } else {
        list.path.is_ident("any") && !items.is_empty() && items.iter().all(requires_test)
    }
}

fn macro_unsafe(tokens: proc_macro2::TokenStream, inventory: &mut Inventory) {
    for token in tokens {
        match token {
            proc_macro2::TokenTree::Ident(ident) if ident == "unsafe" => {
                inventory.record(ident.span().start().line, "unsafe-token-in-macro")
            }
            proc_macro2::TokenTree::Group(group) => macro_unsafe(group.stream(), inventory),
            _ => (),
        }
    }
}

impl<'ast> Visit<'ast> for Inventory {
    fn visit_file(&mut self, file: &'ast syn::File) {
        self.scoped(&file.attrs, |this| visit::visit_file(this, file));
    }
    fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
        self.scoped(&item.attrs, |this| visit::visit_item_const(this, item));
    }
    fn visit_item_static(&mut self, item: &'ast syn::ItemStatic) {
        self.scoped(&item.attrs, |this| visit::visit_item_static(this, item));
    }
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        self.scoped(&item.attrs, |this| visit::visit_item_mod(this, item));
    }
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.scoped(&item.attrs, |this| visit::visit_item_fn(this, item));
    }
    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        self.scoped(&item.attrs, |this| {
            if let Some(token) = item.unsafety {
                this.record(token.span.start().line, "unsafe-impl");
            }
            visit::visit_item_impl(this, item);
        });
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.scoped(&item.attrs, |this| visit::visit_impl_item_fn(this, item));
    }
    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        self.scoped(&item.attrs, |this| {
            if let Some(token) = item.unsafety {
                this.record(token.span.start().line, "unsafe-trait");
            }
            visit::visit_item_trait(this, item);
        });
    }
    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        self.scoped(&item.attrs, |this| visit::visit_trait_item_fn(this, item));
    }
    fn visit_item_foreign_mod(&mut self, item: &'ast syn::ItemForeignMod) {
        self.scoped(&item.attrs, |this| {
            if let Some(token) = item.unsafety {
                this.record(token.span.start().line, "unsafe-extern");
            }
            visit::visit_item_foreign_mod(this, item);
        });
    }
    fn visit_signature(&mut self, signature: &'ast syn::Signature) {
        if let syn::Safety::Unsafe(token) = signature.safety {
            self.record(token.span.start().line, "unsafe-fn");
        }
        visit::visit_signature(self, signature);
    }
    fn visit_expr_unsafe(&mut self, expression: &'ast syn::ExprUnsafe) {
        self.scoped(&expression.attrs, |this| {
            this.record(expression.unsafe_token.span.start().line, "unsafe-block");
            visit::visit_expr_unsafe(this, expression);
        });
    }
    fn visit_macro(&mut self, item: &'ast syn::Macro) {
        // Report every macro location for separate expansion review. Token text
        // scanning cannot establish whether a macro generates unsafe code.
        self.record(item.span().start().line, "unexpanded-macro");
        macro_unsafe(item.tokens.clone(), self);
    }
}

fn rust_files(root: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_dir() {
            rust_files(&path, files)?;
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("src"));
    let mut files = Vec::new();
    rust_files(&root, &mut files)?;
    files.sort();
    println!("path\tline\tkind\tscope\tconditional-attribute-lines");
    for path in files {
        let source = fs::read_to_string(&path)?;
        let ast = syn::parse_file(&source)?;
        let mut inventory = Inventory::default();
        inventory.visit_file(&ast);
        inventory.sites.sort();
        for (line, kind, test, cfg) in inventory.sites {
            println!(
                "{}\t{line}\t{kind}\t{}\t{cfg:?}",
                path.display(),
                if test { "test" } else { "production-candidate" }
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn separates_explicit_test_scopes_and_records_macro_gaps() {
        let ast = syn::parse_file(
            r#"
            unsafe fn production() { unsafe { action() } }
            #[cfg(test)] mod tests { fn fixture() { unsafe { action() } } }
            #[test] fn direct_test() { unsafe { action() } }
            #[cfg(target_os = "linux")] fn platform() { unsafe { action() } }
            generated!();
            #[cfg(all(test, target_os = "linux"))] mod platform_tests { fn helper() { unsafe { action() } } }
            #[cfg(any(test, feature = "live"))] fn also_production() { unsafe { action() } }
            #[test] fn in_macro() { assert!(unsafe { predicate() }); }
        "#,
        )
        .unwrap();
        let mut inventory = Inventory::default();
        inventory.visit_file(&ast);
        assert_eq!(
            inventory
                .sites
                .iter()
                .filter(|site| site.2 && site.1 == "unsafe-block")
                .count(),
            3
        );
        assert_eq!(
            inventory
                .sites
                .iter()
                .filter(|site| site.2 && site.1 == "unsafe-token-in-macro")
                .count(),
            1
        );
        assert_eq!(
            inventory
                .sites
                .iter()
                .filter(|site| site.1 == "unsafe-fn")
                .count(),
            1
        );
        assert_eq!(
            inventory
                .sites
                .iter()
                .filter(|site| site.1 == "unexpanded-macro")
                .count(),
            2
        );
        assert!(inventory
            .sites
            .iter()
            .any(|site| !site.2 && !site.3.is_empty()));
    }
}
