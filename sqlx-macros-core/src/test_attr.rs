use proc_macro2::TokenStream;
use quote::quote;
use syn::{parse::Parser, Meta};

#[cfg(feature = "migrate")]
use proc_macro2::Span;
#[cfg(feature = "migrate")]
use syn::{punctuated::Punctuated, Expr, ExprLit, Lit, LitStr, MetaNameValue, Token};

#[cfg(feature = "migrate")]
struct Args {
    fixtures: Vec<LitStr>,
    #[cfg(feature = "migrate")]
    migrations: MigrationsOpt,
}

#[cfg(feature = "migrate")]
enum MigrationsOpt {
    InferredPath,
    ExplicitPath(LitStr),
    ExplicitMigrator(syn::Path),
    Disabled,
}

type AttributeArgs = syn::punctuated::Punctuated<Meta, syn::Token![,]>;

pub fn expand(args: TokenStream, input: syn::ItemFn) -> crate::Result<TokenStream> {
    let parser = AttributeArgs::parse_terminated;
    let args = parser.parse2(args)?;

    if input.sig.inputs.is_empty() {
        if !args.is_empty() {
            if cfg!(feature = "migrate") {
                return Err(syn::Error::new_spanned(
                    args.first().unwrap(),
                    "control attributes are not allowed unless \
                        the `migrate` feature is enabled and \
                        automatic test DB management is used; see docs",
                )
                .into());
            }

            return Err(syn::Error::new_spanned(
                args.first().unwrap(),
                "control attributes are not allowed unless \
                    automatic test DB management is used; see docs",
            )
            .into());
        }

        return Ok(expand_simple(input));
    }

    #[cfg(feature = "migrate")]
    return expand_advanced(args, input);

    #[cfg(not(feature = "migrate"))]
    return Err(syn::Error::new_spanned(input, "`migrate` feature required").into());
}

fn expand_simple(input: syn::ItemFn) -> TokenStream {
    let ret = &input.sig.output;
    let name = &input.sig.ident;
    let body = &input.block;
    let attrs = &input.attrs;

    quote! {
        #[::core::prelude::v1::test]
        #(#attrs)*
        fn #name() #ret {
            ::sqlx::test_block_on(async { #body })
        }
    }
}

#[cfg(feature = "migrate")]
fn expand_advanced(args: AttributeArgs, input: syn::ItemFn) -> crate::Result<TokenStream> {
    let ret = &input.sig.output;
    let name = &input.sig.ident;
    let inputs = &input.sig.inputs;
    let body = &input.block;
    let attrs = &input.attrs;

    let args = parse_args(args)?;

    let fn_arg_types = inputs.iter().map(|_| quote! { _ });

    let fixtures = args.fixtures.into_iter().map(|fixture| {
        let path = format!("fixtures/{}.sql", fixture.value());

        quote! {
            ::sqlx::testing::TestFixture {
                path: #path,
                contents: include_str!(#path),
            }
        }
    });

    let migrations = match args.migrations {
        MigrationsOpt::ExplicitPath(path) => {
            let migrator = crate::migrate::expand_migrator_from_lit_dir(path)?;
            quote! { args.migrator(&#migrator); }
        }
        MigrationsOpt::InferredPath if !inputs.is_empty() => {
            let migrations_path = crate::common::resolve_path("./migrations", Span::call_site())?;

            if migrations_path.is_dir() {
                let migrator = crate::migrate::expand_migrator(&migrations_path)?;
                quote! { args.migrator(&#migrator); }
            } else {
                quote! {}
            }
        }
        MigrationsOpt::ExplicitMigrator(path) => {
            quote! { args.migrator(&#path); }
        }
        _ => quote! {},
    };

    Ok(quote! {
        #[::core::prelude::v1::test]
        #(#attrs)*
        fn #name() #ret {
            async fn inner(#inputs) #ret {
                #body
            }

            let mut args = ::sqlx::testing::TestArgs::new(concat!(module_path!(), "::", stringify!(#name)));

            #migrations

            args.fixtures(&[#(#fixtures),*]);

            // We need to give a coercion site or else we get "unimplemented trait" errors.
            let f: fn(#(#fn_arg_types),*) -> _ = inner;

            ::sqlx::testing::TestFn::run_test(f, args)
        }
    })
}

#[cfg(feature = "migrate")]
fn parse_args(args: AttributeArgs) -> Result<Args, syn::Error> {
    let mut fixtures = vec![];
    let mut migrations = MigrationsOpt::InferredPath;

    for arg in args {
        let path = arg.path().clone();

        match arg {
            Meta::List(list) if path.is_ident("fixtures") => {
                if !fixtures.is_empty() {
                    return Err(syn::Error::new_spanned(path, "duplicate `fixtures` arg"));
                }

                let parser = <Punctuated<LitStr, Token![,]>>::parse_terminated;
                let list = parser.parse2(list.tokens)?;
                fixtures.extend(list);
            }
            Meta::NameValue(MetaNameValue { value, .. }) if path.is_ident("migrations") => {
                if !matches!(migrations, MigrationsOpt::InferredPath) {
                    return Err(syn::Error::new_spanned(
                        path,
                        "cannot have more than one `migrations` or `migrator` arg",
                    ));
                }

                let Expr::Lit(ExprLit { lit, .. }) = value else {
                    return Err(syn::Error::new_spanned(path, "expected string for `false`"))
                };

                migrations = match lit {
                    // migrations = false
                    Lit::Bool(b) if !b.value => MigrationsOpt::Disabled,
                    // migrations = true
                    Lit::Bool(b) => {
                        return Err(syn::Error::new_spanned(
                            b,
                            "`migrations = true` is redundant",
                        ));
                    }
                    // migrations = "path"
                    Lit::Str(s) => MigrationsOpt::ExplicitPath(s),
                    lit => return Err(syn::Error::new_spanned(lit, "expected string or `false`")),
                };
            }
            // migrator = "path"
            Meta::NameValue(MetaNameValue { value, .. }) if path.is_ident("migrator") => {
                if !matches!(migrations, MigrationsOpt::InferredPath) {
                    return Err(syn::Error::new_spanned(
                        path,
                        "cannot have more than one `migrations` or `migrator` arg",
                    ));
                }

                let Expr::Lit(ExprLit { lit: Lit::Str(lit), .. }) = value else {
                    return Err(syn::Error::new_spanned(path, "expected string"))
                };

                migrations = MigrationsOpt::ExplicitMigrator(lit.parse()?);
            }
            arg => {
                return Err(syn::Error::new_spanned(
                    arg,
                    r#"expected `fixtures("<filename>", ...)` or `migrations = "<path>" | false` or `migrator = "<rust path>"`"#,
                ));
            }
        };
    }

    Ok(Args {
        fixtures,
        migrations,
    })
}
