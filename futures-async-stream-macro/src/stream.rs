use proc_macro2::TokenStream;
use quote::{quote, ToTokens};
use syn::{
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
    token,
    visit_mut::VisitMut,
    Block, ExprAsync, FnArg, Pat, PatIdent, PatType, Result, Signature, Stmt, Type,
};

use crate::{
    elision,
    parse::{self, Context, FnOrAsync, FnSig},
    utils::{parse_as_empty, TASK_CONTEXT},
    visitor::Visitor,
};

mod kw {
    syn::custom_keyword!(item);
    syn::custom_keyword!(ok);
    syn::custom_keyword!(error);
    syn::custom_keyword!(boxed);
    syn::custom_keyword!(boxed_local);
}

pub(crate) fn attribute(args: TokenStream, input: TokenStream, cx: Context) -> Result<TokenStream> {
    match parse::parse(input, cx)? {
        FnOrAsync::Fn(sig) => parse_fn(args, sig, cx),
        FnOrAsync::Async(mut expr, semi) => {
            parse_as_empty(&args)?;
            let mut tokens = parse_async(&mut expr, cx)?;
            if let Some(semi) = semi {
                semi.to_tokens(&mut tokens);
            }
            Ok(tokens)
        }
        FnOrAsync::NotAsync => unreachable!(),
    }
}

pub(crate) fn parse_async(expr: &mut ExprAsync, cx: Context) -> Result<TokenStream> {
    Visitor::new(cx.into()).visit_expr_async_mut(expr);
    Ok(make_gen_body(expr.capture, &expr.block, cx, None, false))
}

#[allow(dead_code)] // fixed in latest nightly
#[derive(Clone, Copy)]
enum ReturnTypeKind {
    // impl Stream<Item = ..> $(+ $lifetime)?
    Default,
    // Pin<Box<dyn Stream<Item = ..> (+ Send)? $(+ $lifetime)?>>
    Boxed { send: bool },
}

impl ReturnTypeKind {
    fn parse_or_else<F>(&mut self, input: ParseStream<'_>, mut f: F) -> Result<()>
    where
        F: FnMut(ParseStream<'_>) -> Result<()>,
    {
        while !input.is_empty() {
            if input.peek(kw::boxed) {
                let i: kw::boxed = input.parse()?;
                match self {
                    Self::Default => *self = Self::Boxed { send: true },
                    Self::Boxed { send: true } => {
                        return Err(error!(i, "duplicate `boxed` argument"));
                    }
                    Self::Boxed { send: false } => {
                        return Err(error!(
                            i,
                            "`boxed` and `boxed_local` may not be used at the same time"
                        ));
                    }
                }
            } else if input.peek(kw::boxed_local) {
                let i: kw::boxed_local = input.parse()?;
                match self {
                    Self::Default => *self = Self::Boxed { send: false },
                    Self::Boxed { send: false } => {
                        return Err(error!(i, "duplicate `boxed_local` argument"));
                    }
                    Self::Boxed { send: true } => {
                        return Err(error!(
                            i,
                            "`boxed` and `boxed_local` may not be used at the same time"
                        ));
                    }
                }
            } else {
                f(input)?;
            }

            if !input.is_empty() {
                let _: token::Comma = input.parse()?;
            }
        }

        Ok(())
    }

    fn is_boxed(self) -> bool {
        if let Self::Boxed { .. } = self { true } else { false }
    }
}

// Replace `prev` with `new`. Returns `Err` if `prev` is `Some`.
fn replace<T>(prev: &mut Option<T>, new: T, token: &impl ToTokens) -> Result<()> {
    if prev.replace(new).is_some() {
        Err(error!(token, "duplicate `{}` argument", token.to_token_stream()))
    } else {
        Ok(())
    }
}

struct StreamArg {
    item_ty: Type,
    boxed: ReturnTypeKind,
}

impl Parse for StreamArg {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut item_ty = None;
        let mut boxed = ReturnTypeKind::Default;
        boxed.parse_or_else(input, |input| {
            if input.peek(kw::item) {
                // item = <Type>
                let i: kw::item = input.parse()?;
                let _: token::Eq = input.parse()?;
                replace(&mut item_ty, input.parse()?, &i)
            } else if item_ty.is_none() {
                input.parse::<kw::item>().map(|_| unreachable!())
            } else {
                let token = input.parse::<TokenStream>()?;
                Err(error!(token, "unexpected argument: {}", token))
            }
        })?;

        if let Some(item_ty) = item_ty {
            Ok(Self { item_ty, boxed })
        } else {
            input.parse::<kw::item>().map(|_| unreachable!())
        }
    }
}

struct TryStreamArg {
    ok: Type,
    error: Type,
    boxed: ReturnTypeKind,
}

impl Parse for TryStreamArg {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut ok = None;
        let mut error = None;
        let mut boxed = ReturnTypeKind::Default;
        boxed.parse_or_else(input, |input| {
            if input.peek(kw::ok) {
                // ok = <Type>
                let i: kw::ok = input.parse()?;
                let _: token::Eq = input.parse()?;
                replace(&mut ok, input.parse()?, &i)
            } else if input.peek(kw::error) {
                // error = <Type>
                let i: kw::error = input.parse()?;
                let _: token::Eq = input.parse()?;
                replace(&mut error, input.parse()?, &i)
            } else if ok.is_none() {
                input.parse::<kw::ok>().map(|_| unreachable!())
            } else if error.is_none() {
                input.parse::<kw::error>().map(|_| unreachable!())
            } else {
                let token = input.parse::<TokenStream>()?;
                Err(error!(token, "unexpected argument: {}", token))
            }
        })?;

        match (ok, error) {
            (Some(ok), Some(error)) => Ok(Self { ok, error, boxed }),
            (Some(_), None) => input.parse::<kw::error>().map(|_| unreachable!()),
            (None, _) => input.parse::<kw::ok>().map(|_| unreachable!()),
        }
    }
}

fn parse_fn(args: TokenStream, sig: FnSig, cx: Context) -> Result<TokenStream> {
    match cx {
        Context::Stream => {
            let StreamArg { item_ty, boxed } = syn::parse2(args)?;
            parse_fn_inner(sig, cx, None, boxed.is_boxed(), |lifetimes| match boxed {
                ReturnTypeKind::Default => {
                    // Raw `impl` breaks syntax highlighting in some editors.
                    let impl_token = token::Impl::default();
                    quote! {
                        #impl_token ::futures_async_stream::__reexport::stream::Stream<Item = #item_ty> + #lifetimes
                    }
                }
                ReturnTypeKind::Boxed { send } => {
                    let send = if send {
                        quote!(+ ::futures_async_stream::__reexport::marker::Send)
                    } else {
                        TokenStream::new()
                    };
                    quote! {
                        ::futures_async_stream::__reexport::pin::Pin<Box<
                            dyn ::futures_async_stream::__reexport::stream::Stream<Item = #item_ty> #send + #lifetimes
                        >>
                    }
                }
            })
        }
        Context::TryStream => {
            let TryStreamArg { ok, error, boxed } = syn::parse2(args)?;
            parse_fn_inner(sig, cx, Some(&error), boxed.is_boxed(), |lifetimes| {
                match boxed {
                    ReturnTypeKind::Default => {
                        // Raw `impl` breaks syntax highlighting in some editors.
                        let impl_token = token::Impl::default();
                        quote! {
                            #impl_token ::futures_async_stream::__reexport::stream::Stream<
                                Item = ::futures_async_stream::__reexport::result::Result<#ok, #error>
                            > + #lifetimes
                        }
                    }
                    ReturnTypeKind::Boxed { send } => {
                        let send = if send {
                            quote!(+ ::futures_async_stream::__reexport::marker::Send)
                        } else {
                            TokenStream::new()
                        };
                        quote! {
                            ::futures_async_stream::__reexport::pin::Pin<Box<
                                dyn ::futures_async_stream::__reexport::stream::Stream<
                                    Item = ::futures_async_stream::__reexport::result::Result<#ok, #error>
                                > #send + #lifetimes
                            >>
                        }
                    }
                }
            })
        }
    }
}

fn parse_fn_inner(
    sig: FnSig,
    cx: Context,
    error: Option<&Type>,
    boxed: bool,
    return_ty: impl FnOnce(TokenStream) -> TokenStream,
) -> Result<TokenStream> {
    let FnSig { attrs, vis, sig, mut block, semi } = sig;
    let Signature { unsafety, abi, fn_token, ident, mut generics, inputs, .. } = sig;
    let where_clause = &generics.where_clause;

    // Visit `#[for_await]`, `.await`, and `yield`.
    Visitor::new(cx.into()).visit_block_mut(&mut block);

    let (mut arguments, mut statements) = expand_async_body(inputs);
    statements.append(&mut block.stmts);
    block.stmts = statements;

    let body_inner = make_gen_body(Some(token::Move::default()), &block, cx, error, boxed);
    let mut body = TokenStream::new();
    block.brace_token.surround(&mut body, |tokens| {
        body_inner.to_tokens(tokens);
    });

    elision::unelide_lifetimes(&mut generics.params, &mut arguments);
    let lifetimes = generics.lifetimes().map(|l| &l.lifetime);
    let return_ty = return_ty(quote!(#(#lifetimes +)*));

    let body = semi.map_or(body, ToTokens::into_token_stream);
    Ok(quote! {
        #(#attrs)*
        #vis #unsafety #abi #fn_token #ident #generics (#(#arguments),*) -> #return_ty
        #where_clause
        #body
    })
}

// https://github.com/rust-lang/rust/pull/61413/
fn expand_async_body(inputs: Punctuated<FnArg, token::Comma>) -> (Vec<FnArg>, Vec<Stmt>) {
    let mut arguments = Vec::new();
    let mut statements = Vec::new();

    // Desugar `async fn`
    // from:
    //
    //      #[stream(item = u32)]
    //      async fn foo(<pattern>: <ty>, <pattern>: <ty>, <pattern>: <ty>) {
    //          // ...
    //      }
    //
    // into:
    //
    //      fn foo(__arg0: <ty>, __arg1: <ty>, __arg2: <ty>) -> impl Stream<Item = u32> {
    //          from_generator(static move || {
    //              let mut __arg2 = __arg2;
    //              let <pattern> = __arg2;
    //              let mut __arg1 = __arg1;
    //              let <pattern> = __arg1;
    //              let mut __arg0 = __arg0;
    //              let <pattern> = __arg0;
    //              // ...
    //          })
    //      }
    //
    // If `<pattern>` is a simple ident, then it is lowered to a single
    // `let <pattern> = <pattern>;` statement as an optimization.
    for (i, argument) in inputs.into_iter().enumerate() {
        if let FnArg::Typed(PatType { attrs, pat, ty, colon_token }) = argument {
            if let Type::Reference(_) = &*ty {
                arguments.push(FnArg::Typed(PatType { attrs, pat, ty, colon_token }));
                continue;
            }

            let is_simple_argument = match &*pat {
                // We notably skip everything related to `self` which typically doesn't have
                // many patterns with it and just gets captured naturally.
                Pat::Ident(PatIdent { ident, .. }) if ident == "self" => {
                    arguments.push(FnArg::Typed(PatType { attrs, pat, ty, colon_token }));
                    continue;
                }
                Pat::Ident(PatIdent { by_ref: None, mutability: None, .. }) => true,
                _ => false,
            };

            if is_simple_argument {
                // If this is the simple case, then we only insert one statement that is
                // `let <pat> = <pat>;`.
                statements.push(syn::parse_quote!(let #pat = #pat;));

                arguments.push(FnArg::Typed(PatType { attrs, pat, ty, colon_token }));
                continue;
            }

            // If this is not the simple case, then we construct two statements:
            //
            // ```
            // let mut __argN = __argN;
            // let <pat> = __argN;
            // ```
            //
            // The first statement moves the argument into the closure and thus ensures
            // that the drop order is correct.
            //
            // The second statement creates the bindings that the user wrote.

            let ident = def_site_ident!("__arg{}", i);

            // Construct the `let mut __argN = __argN;` statement. It must be a mut binding
            // because the user may have specified a `ref mut` binding in the next
            // statement.
            statements.push(syn::parse_quote!(let mut #ident = #ident;));

            // Construct the `let <pat> = __argN;` statement.
            statements.push(syn::parse_quote!(let #pat = #ident;));

            let pat = Box::new(Pat::Ident(PatIdent {
                attrs: Vec::new(),
                by_ref: None,
                mutability: None,
                ident,
                subpat: None,
            }));
            arguments.push(FnArg::Typed(PatType { attrs, pat, ty, colon_token }));
        } else {
            arguments.push(argument);
        }
    }

    (arguments, statements)
}

fn make_gen_body(
    capture: Option<token::Move>,
    block: &Block,
    cx: Context,
    error: Option<&Type>,
    boxed: bool,
) -> TokenStream {
    let (gen_function, ret_value, ret_ty) = match cx {
        Context::Stream => (
            quote!(::futures_async_stream::__reexport::stream::from_generator),
            TokenStream::new(),
            quote!(()),
        ),
        Context::TryStream => {
            let error = error.map_or_else(|| quote!(_), ToTokens::to_token_stream);
            (
                quote!(::futures_async_stream::__reexport::try_stream::from_generator),
                quote!(::futures_async_stream::__reexport::result::Result::Ok(())),
                quote!(::futures_async_stream::__reexport::result::Result<(), #error>),
            )
        }
    };

    let task_context = def_site_ident!(TASK_CONTEXT);
    let body = quote! {
        #gen_function(
            static #capture |mut #task_context: ::futures_async_stream::__reexport::future::ResumeTy| -> #ret_ty {
                let (): () = #block;

                // Ensure that this closure is a generator, even if it doesn't
                // have any `yield` statements.
                #[allow(unreachable_code)]
                {
                    return #ret_value;
                    loop { #task_context = yield ::futures_async_stream::__reexport::task::Poll::Pending }
                }
            }
        )
    };

    if boxed { quote!(Box::pin(#body)) } else { body }
}
