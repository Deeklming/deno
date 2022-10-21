// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.

use core::panic;
use once_cell::sync::Lazy;
use proc_macro::TokenStream;
use proc_macro2::Span;
use proc_macro2::TokenStream as TokenStream2;
use proc_macro_crate::crate_name;
use proc_macro_crate::FoundCrate;
use quote::format_ident;
use quote::quote;
use quote::ToTokens;
use regex::Regex;
use std::collections::HashMap;
use syn::punctuated::Punctuated;
use syn::token::Comma;
use syn::FnArg;
use syn::GenericParam;
use syn::Ident;

#[cfg(test)]
mod tests;

// Identifier to the `deno_core` crate.
//
// If macro called in deno_core, `crate` is used.
// If macro called outside deno_core, `deno_core` OR the renamed
// version from Cargo.toml is used.
fn core_import() -> TokenStream2 {
  let found_crate =
    crate_name("deno_core").expect("deno_core not present in `Cargo.toml`");

  match found_crate {
    FoundCrate::Itself => {
      // TODO(@littledivy): This won't work for `deno_core` examples
      // since `crate` does not refer to `deno_core`.
      // examples must re-export deno_core to make this work
      // until Span inspection APIs are stabalized.
      //
      // https://github.com/rust-lang/rust/issues/54725
      quote!(crate)
    }
    FoundCrate::Name(name) => {
      let ident = Ident::new(&name, Span::call_site());
      quote!(#ident)
    }
  }
}

#[derive(Copy, Clone, Debug, Default)]
struct MacroArgs {
  is_unstable: bool,
  is_v8: bool,
  must_be_fast: bool,
  deferred: bool,
}

impl syn::parse::Parse for MacroArgs {
  fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
    let vars =
      syn::punctuated::Punctuated::<Ident, syn::Token![,]>::parse_terminated(
        input,
      )?;
    let vars: Vec<_> = vars.iter().map(Ident::to_string).collect();
    let vars: Vec<_> = vars.iter().map(String::as_str).collect();
    for var in vars.iter() {
      if !["unstable", "v8", "fast", "deferred"].contains(var) {
        return Err(syn::Error::new(
          input.span(),
          "Ops expect #[op] or #[op(unstable)]",
        ));
      }
    }
    Ok(Self {
      is_unstable: vars.contains(&"unstable"),
      is_v8: vars.contains(&"v8"),
      must_be_fast: vars.contains(&"fast"),
      deferred: vars.contains(&"deferred"),
    })
  }
}

#[proc_macro_attribute]
pub fn op(attr: TokenStream, item: TokenStream) -> TokenStream {
  let margs = syn::parse_macro_input!(attr as MacroArgs);
  let MacroArgs {
    is_unstable,
    is_v8,
    must_be_fast,
    deferred,
  } = margs;
  let func = syn::parse::<syn::ItemFn>(item).expect("expected a function");
  let name = &func.sig.ident;
  let mut generics = func.sig.generics.clone();
  let scope_lifetime =
    syn::LifetimeDef::new(syn::Lifetime::new("'scope", Span::call_site()));
  if !generics.lifetimes().any(|def| *def == scope_lifetime) {
    generics
      .params
      .push(syn::GenericParam::Lifetime(scope_lifetime));
  }
  let type_params = exclude_lifetime_params(&func.sig.generics.params);
  let where_clause = &func.sig.generics.where_clause;

  // Preserve the original func as op_foo::call()
  let original_func = {
    let mut func = func.clone();
    func.sig.ident = quote::format_ident!("call");
    func
  };

  let core = core_import();

  let asyncness = func.sig.asyncness.is_some();
  let is_async = asyncness || is_future(&func.sig.output);

  // First generate fast call bindings to opt-in to error handling in slow call
  let (has_fallible_fast_call, fast_impl, fast_field) =
    codegen_fast_impl(&core, &func, name, is_async, must_be_fast);

  let v8_body = if is_async {
    codegen_v8_async(&core, &func, margs, asyncness, deferred)
  } else {
    codegen_v8_sync(&core, &func, margs, has_fallible_fast_call)
  };

  let docline = format!("Use `{name}::decl()` to get an op-declaration");
  // Generate wrapper
  quote! {
    #[allow(non_camel_case_types)]
    #[doc="Auto-generated by `deno_ops`, i.e: `#[op]`"]
    #[doc=""]
    #[doc=#docline]
    #[doc="you can include in a `deno_core::Extension`."]
    pub struct #name;

    #[doc(hidden)]
    impl #name {
      pub fn name() -> &'static str {
        stringify!(#name)
      }

      pub fn v8_fn_ptr #generics () -> #core::v8::FunctionCallback #where_clause {
        use #core::v8::MapFnTo;
        Self::v8_func::<#type_params>.map_fn_to()
      }

      pub fn decl #generics () -> #core::OpDecl #where_clause {
        #core::OpDecl {
          name: Self::name(),
          v8_fn_ptr: Self::v8_fn_ptr::<#type_params>(),
          enabled: true,
          fast_fn: #fast_field,
          is_async: #is_async,
          is_unstable: #is_unstable,
          is_v8: #is_v8,
        }
      }

      #[inline]
      #[allow(clippy::too_many_arguments)]
      #original_func

      pub fn v8_func #generics (
        scope: &mut #core::v8::HandleScope<'scope>,
        args: #core::v8::FunctionCallbackArguments,
        mut rv: #core::v8::ReturnValue,
      ) #where_clause {
        #v8_body
      }
    }

    #fast_impl
  }.into()
}

/// Generate the body of a v8 func for an async op
fn codegen_v8_async(
  core: &TokenStream2,
  f: &syn::ItemFn,
  margs: MacroArgs,
  asyncness: bool,
  deferred: bool,
) -> TokenStream2 {
  let MacroArgs { is_v8, .. } = margs;
  let special_args = f
    .sig
    .inputs
    .iter()
    .map_while(|a| {
      (if is_v8 { scope_arg(a) } else { None }).or_else(|| opstate_arg(a))
    })
    .collect::<Vec<_>>();
  let rust_i0 = special_args.len();
  let args_head = special_args.into_iter().collect::<TokenStream2>();

  let (arg_decls, args_tail) = codegen_args(core, f, rust_i0, 1);
  let type_params = exclude_lifetime_params(&f.sig.generics.params);

  let (pre_result, mut result_fut) = match asyncness {
    true => (
      quote! {},
      quote! { Self::call::<#type_params>(#args_head #args_tail).await; },
    ),
    false => (
      quote! { let result_fut = Self::call::<#type_params>(#args_head #args_tail); },
      quote! { result_fut.await; },
    ),
  };
  let result_wrapper = match is_result(&f.sig.output) {
    true => {
      // Support `Result<impl Future<Output = Result<T, AnyError>> + 'static, AnyError>`
      if !asyncness {
        result_fut = quote! { result_fut; };
        quote! {
          let result = match result {
            Ok(fut) => fut.await,
            Err(e) => return (promise_id, op_id, #core::_ops::to_op_result::<()>(get_class, Err(e))),
          };
        }
      } else {
        quote! {}
      }
    }
    false => quote! { let result = Ok(result); },
  };

  quote! {
    use #core::futures::FutureExt;
    // SAFETY: #core guarantees args.data() is a v8 External pointing to an OpCtx for the isolates lifetime
    let ctx = unsafe {
      &*(#core::v8::Local::<#core::v8::External>::cast(args.data()).value()
      as *const #core::_ops::OpCtx)
    };
    let op_id = ctx.id;

    let promise_id = args.get(0);
    let promise_id = #core::v8::Local::<#core::v8::Integer>::try_from(promise_id)
      .map(|l| l.value() as #core::PromiseId)
      .map_err(#core::anyhow::Error::from);
    // Fail if promise id invalid (not an int)
    let promise_id: #core::PromiseId = match promise_id {
      Ok(promise_id) => promise_id,
      Err(err) => {
        #core::_ops::throw_type_error(scope, format!("invalid promise id: {}", err));
        return;
      }
    };

    #arg_decls

    // Track async call & get copy of get_error_class_fn
    let get_class = {
      let state = ::std::cell::RefCell::borrow(&ctx.state);
      state.tracker.track_async(op_id);
      state.get_error_class_fn
    };

    #pre_result
    #core::_ops::queue_async_op(ctx, scope, #deferred, async move {
      let result = #result_fut
      #result_wrapper
      (promise_id, op_id, #core::_ops::to_op_result(get_class, result))
    });
  }
}

fn scope_arg(arg: &FnArg) -> Option<TokenStream2> {
  if is_handle_scope(arg) {
    Some(quote! { scope, })
  } else {
    None
  }
}

fn opstate_arg(arg: &FnArg) -> Option<TokenStream2> {
  match arg {
    arg if is_rc_refcell_opstate(arg) => Some(quote! { ctx.state.clone(), }),
    arg if is_mut_ref_opstate(arg) => {
      Some(quote! { &mut std::cell::RefCell::borrow_mut(&ctx.state), })
    }
    _ => None,
  }
}

fn codegen_fast_impl(
  core: &TokenStream2,
  f: &syn::ItemFn,
  name: &syn::Ident,
  is_async: bool,
  must_be_fast: bool,
) -> (bool, TokenStream2, TokenStream2) {
  if is_async {
    if must_be_fast {
      panic!("async op cannot be a fast api. enforced by #[op(fast)]")
    }
    return (false, quote! {}, quote! { None });
  }
  let fast_info = can_be_fast_api(core, f);
  if must_be_fast && fast_info.is_none() {
    panic!("op cannot be a fast api. enforced by #[op(fast)]")
  }
  if !is_async {
    if let Some(FastApiSyn {
      args,
      ret,
      use_op_state,
      use_fast_cb_opts,
      v8_values,
      returns_result,
      slices,
    }) = fast_info
    {
      let offset = if use_op_state { 1 } else { 0 };
      let mut inputs = f
        .sig
        .inputs
        .iter()
        .skip(offset)
        .enumerate()
        .map(|(idx, arg)| {
          let ident = match arg {
            FnArg::Receiver(_) => unreachable!(),
            FnArg::Typed(t) => match &*t.pat {
              syn::Pat::Ident(i) => format_ident!("{}", i.ident),
              _ => unreachable!(),
            },
          };
          if let Some(ty) = slices.get(&(idx + offset)) {
            return quote! { #ident: *const #core::v8::fast_api::FastApiTypedArray< #ty > };
          }
          if use_fast_cb_opts && idx + offset == f.sig.inputs.len() - 1 {
            return quote! { fast_api_callback_options: *mut #core::v8::fast_api::FastApiCallbackOptions };
          }
          if v8_values.contains(&idx) {
            return quote! { #ident: #core::v8::Local < #core::v8::Value > };
          }
          quote!(#arg)
        })
        .collect::<Vec<_>>();
      if (!slices.is_empty() || use_op_state || returns_result)
        && !use_fast_cb_opts
      {
        inputs.push(quote! { fast_api_callback_options: *mut #core::v8::fast_api::FastApiCallbackOptions });
      }
      let input_idents = f
        .sig
        .inputs
        .iter()
        .enumerate()
        .map(|(idx, a)| {
          let ident = match a {
            FnArg::Receiver(_) => unreachable!(),
            FnArg::Typed(t) => match &*t.pat {
              syn::Pat::Ident(i) => format_ident!("{}", i.ident),
              _ => unreachable!(),
            },
          };
          if slices.get(&idx).is_some() {
            return quote! {
              match unsafe { &* #ident }.get_storage_if_aligned() {
                Some(s) => s,
                None => {
                  unsafe { &mut * fast_api_callback_options }.fallback = true;
                  return Default::default();
                },
              }
            };
          }
          if use_fast_cb_opts && idx == f.sig.inputs.len() - 1 {
            return quote! { Some(unsafe { &mut * fast_api_callback_options }) };
          }
          if v8_values.contains(&idx) {
            return quote! {
              #core::serde_v8::Value {
                v8_value: #ident,
              }
            };
          }
          quote! { #ident }
        })
        .collect::<Vec<_>>();
      let generics = &f.sig.generics;
      let (impl_generics, ty_generics, where_clause) =
        generics.split_for_impl();
      let type_params = exclude_lifetime_params(&f.sig.generics.params);
      let (trampoline, raw_block) = if is_async {
        // TODO(@littledivy): Fast async calls.
        (
          quote! {
            fn func(recv: #core::v8::Local<#core::v8::Object>, __promise_id: u32, #(#inputs),*) {
              // SAFETY: V8 calling convention guarantees that the callback options pointer is non-null.
              let opts: &#core::v8::fast_api::FastApiCallbackOptions = unsafe { &*fast_api_callback_options };
              // SAFETY: data union is always created as the `v8::Local<v8::Value>` version
              let data = unsafe { opts.data.data };
              // SAFETY: #core guarantees data is a v8 External pointing to an OpCtx for the isolates lifetime
              let ctx = unsafe {
                &*(#core::v8::Local::<#core::v8::External>::cast(data).value()
                as *const #core::_ops::OpCtx)
              };
              let op_id = ctx.op_id;
              #core::_ops::queue_async_op(scope, async move {
                let result = Self::call(#args);
                (__promise_id, __op_id, #core::_ops::OpResult::Ok(result))
              });
            }
            func as *const _
          },
          quote! {},
        )
      } else {
        let output = if returns_result {
          get_fast_result_return_type(&f.sig.output)
        } else {
          let output = &f.sig.output;
          quote! { #output }
        };
        let func_name = format_ident!("func_{}", name);
        let op_state_name = if use_op_state {
          input_idents.first().unwrap().clone()
        } else {
          quote! { op_state }
        };
        let recv_decl = if use_op_state || returns_result {
          quote! {
            // SAFETY: V8 calling convention guarantees that the callback options pointer is non-null.
            let opts: &mut #core::v8::fast_api::FastApiCallbackOptions = unsafe { &mut *fast_api_callback_options };
            // SAFETY: data union is always created as the `v8::Local<v8::Value>` version.
            let data = unsafe { opts.data.data };
            // SAFETY: #core guarantees data is a v8 External pointing to an OpCtx for the isolates lifetime
            let ctx = unsafe {
              &*(#core::v8::Local::<#core::v8::External>::cast(data).value()
              as *const #core::_ops::OpCtx)
            };
            let #op_state_name = &mut std::cell::RefCell::borrow_mut(&ctx.state);
          }
        } else {
          quote! {}
        };

        let result_handling = if returns_result {
          quote! {
            match result {
              Ok(result) => {
                result
              },
              Err(err) => {
                #op_state_name.last_fast_op_error.replace(err);
                opts.fallback = true;
                Default::default()
              },
            }
          }
        } else {
          quote! { result }
        };

        (
          quote! {
            fn #func_name #generics (_recv: #core::v8::Local<#core::v8::Object>, #(#inputs),*) #output #where_clause {
              #recv_decl
              let result = #name::call::<#type_params>(#(#input_idents),*);
              #result_handling
            }
          },
          quote! {
            #func_name::<#type_params> as *const _
          },
        )
      };

      let fast_struct = format_ident!("fast_{}", name);
      let (type_params, ty_generics, struct_generics) =
        if type_params.is_empty() {
          (quote! { () }, quote! {}, quote! {})
        } else {
          (
            quote! { #type_params },
            quote! { #ty_generics },
            quote! { ::<#type_params> },
          )
        };
      return (
        returns_result,
        quote! {
          #[allow(non_camel_case_types)]
          #[doc(hidden)]
          struct #fast_struct #ty_generics {
            _phantom: ::std::marker::PhantomData<#type_params>,
          }
          #trampoline
          impl #impl_generics #core::v8::fast_api::FastFunction for #fast_struct #ty_generics #where_clause {
            fn function(&self) -> *const ::std::ffi::c_void  {
              #raw_block
            }
            fn args(&self) -> &'static [#core::v8::fast_api::Type] {
              &[ #args ]
            }
            fn return_type(&self) -> #core::v8::fast_api::CType {
              #ret
            }
          }
        },
        quote! { Some(Box::new(#fast_struct #struct_generics { _phantom: ::std::marker::PhantomData })) },
      );
    }
  }

  // Default impl to satisfy generic bounds for non-fast ops
  (false, quote! {}, quote! { None })
}

/// Generate the body of a v8 func for a sync op
fn codegen_v8_sync(
  core: &TokenStream2,
  f: &syn::ItemFn,
  margs: MacroArgs,
  has_fallible_fast_call: bool,
) -> TokenStream2 {
  let MacroArgs { is_v8, .. } = margs;
  let special_args = f
    .sig
    .inputs
    .iter()
    .map_while(|a| {
      (if is_v8 { scope_arg(a) } else { None }).or_else(|| opstate_arg(a))
    })
    .collect::<Vec<_>>();
  let rust_i0 = special_args.len();
  let args_head = special_args.into_iter().collect::<TokenStream2>();
  let (arg_decls, args_tail) = codegen_args(core, f, rust_i0, 0);
  let ret = codegen_sync_ret(core, &f.sig.output);
  let type_params = exclude_lifetime_params(&f.sig.generics.params);

  let fast_error_handler = if has_fallible_fast_call {
    quote! {
      {
        let op_state = &mut std::cell::RefCell::borrow_mut(&ctx.state);
        if let Some(err) = op_state.last_fast_op_error.take() {
          let exception = #core::error::to_v8_error(scope, op_state.get_error_class_fn, &err);
          scope.throw_exception(exception);
          return;
        }
      }
    }
  } else {
    quote! {}
  };

  quote! {
    // SAFETY: #core guarantees args.data() is a v8 External pointing to an OpCtx for the isolates lifetime
    let ctx = unsafe {
      &*(#core::v8::Local::<#core::v8::External>::cast(args.data()).value()
      as *const #core::_ops::OpCtx)
    };

    #fast_error_handler
    #arg_decls

    let result = Self::call::<#type_params>(#args_head #args_tail);

    // use RefCell::borrow instead of state.borrow to avoid clash with std::borrow::Borrow
    let op_state = ::std::cell::RefCell::borrow(&*ctx.state);
    op_state.tracker.track_sync(ctx.id);

    #ret
  }
}

struct FastApiSyn {
  args: TokenStream2,
  ret: TokenStream2,
  use_op_state: bool,
  use_fast_cb_opts: bool,
  v8_values: Vec<usize>,
  returns_result: bool,
  slices: HashMap<usize, TokenStream2>,
}

fn can_be_fast_api(core: &TokenStream2, f: &syn::ItemFn) -> Option<FastApiSyn> {
  let inputs = &f.sig.inputs;
  let mut returns_result = false;
  let ret = match &f.sig.output {
    syn::ReturnType::Default => quote!(#core::v8::fast_api::CType::Void),
    syn::ReturnType::Type(_, ty) => match is_fast_return_type(core, ty) {
      Some((ret, is_result)) => {
        returns_result = is_result;
        ret
      }
      None => return None,
    },
  };

  let mut use_op_state = false;
  let mut use_fast_cb_opts = false;
  let mut v8_values = Vec::new();
  let mut slices = HashMap::new();
  let mut args = vec![quote! { #core::v8::fast_api::Type::V8Value }];
  for (pos, input) in inputs.iter().enumerate() {
    if pos == inputs.len() - 1 && is_optional_fast_callback_option(input) {
      use_fast_cb_opts = true;
      continue;
    }

    if pos == 0 && is_mut_ref_opstate(input) {
      use_op_state = true;
      continue;
    }

    let ty = match input {
      syn::FnArg::Typed(pat) => &pat.ty,
      _ => unreachable!(),
    };

    if let Some(arg) = is_fast_v8_value(core, ty) {
      args.push(arg);
      v8_values.push(pos);
    } else {
      match is_fast_scalar(core, ty, false) {
        None => match is_fast_arg_sequence(core, ty) {
          Some(arg) => {
            args.push(arg);
          }
          None => match is_ref_slice(&ty) {
            Some(SliceType::U32Mut) => {
              args.push(quote! { #core::v8::fast_api::Type::TypedArray(#core::v8::fast_api::CType::Uint32) });
              slices.insert(pos, quote!(u32));
            }
            Some(_) => {
              args.push(quote! { #core::v8::fast_api::Type::TypedArray(#core::v8::fast_api::CType::Uint8) });
              slices.insert(pos, quote!(u8));
            }
            // early return, this function cannot be a fast call.
            None => return None,
          },
        },
        Some(arg) => {
          args.push(arg);
        }
      }
    }
  }

  if use_fast_cb_opts || use_op_state {
    // Push CallbackOptions into args; it must be the last argument.
    args.push(quote! { #core::v8::fast_api::Type::CallbackOptions });
  }

  let args = args
    .iter()
    .map(|arg| format!("{}", arg))
    .collect::<Vec<_>>()
    .join(", ");
  Some(FastApiSyn {
    args: args.parse().unwrap(),
    ret,
    use_op_state,
    slices,
    v8_values,
    use_fast_cb_opts,
    returns_result,
  })
}

// A v8::Local<v8::Array> or FastApiTypedArray<T>
fn is_fast_arg_sequence(
  core: &TokenStream2,
  ty: impl ToTokens,
) -> Option<TokenStream2> {
  // TODO(@littledivy): Make `v8::` parts optional.
  if is_fast_typed_array(&ty) {
    return Some(
      quote! { #core::v8::fast_api::Type::TypedArray(#core::v8::fast_api::CType::Uint32) },
    );
  }
  if is_local_array(&ty) {
    return Some(
      quote! { #core::v8::fast_api::Type::Sequence(#core::v8::fast_api::CType::Void) },
    );
  }
  None
}

fn is_fast_v8_value(
  core: &TokenStream2,
  arg: impl ToTokens,
) -> Option<TokenStream2> {
  if tokens(&arg).contains("serde_v8 :: Value") {
    return Some(quote! { #core::v8::fast_api::Type::V8Value });
  }
  None
}

fn is_local_array(arg: impl ToTokens) -> bool {
  static RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^v8::Local<v8::Array>$").unwrap());
  RE.is_match(&tokens(arg))
}

fn is_fast_typed_array(arg: impl ToTokens) -> bool {
  static RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#": (?:deno_core :: )?FastApiTypedArray$"#).unwrap()
  });
  RE.is_match(&tokens(arg))
}

fn is_fast_return_type(
  core: &TokenStream2,
  ty: impl ToTokens,
) -> Option<(TokenStream2, bool)> {
  if is_result(&ty) {
    if tokens(&ty).contains("Result < u32") || is_resource_id(&ty) {
      Some((quote! { #core::v8::fast_api::CType::Uint32 }, true))
    } else if tokens(&ty).contains("Result < i32") {
      Some((quote! { #core::v8::fast_api::CType::Int32 }, true))
    } else if tokens(&ty).contains("Result < f32") {
      Some((quote! { #core::v8::fast_api::CType::Float32 }, true))
    } else if tokens(&ty).contains("Result < f64") {
      Some((quote! { #core::v8::fast_api::CType::Float64 }, true))
    } else if tokens(&ty).contains("Result < bool") {
      Some((quote! { #core::v8::fast_api::CType::Bool }, true))
    } else if tokens(&ty).contains("Result < ()") {
      Some((quote! { #core::v8::fast_api::CType::Void }, true))
    } else {
      None
    }
  } else {
    is_fast_scalar(core, ty, true).map(|s| (s, false))
  }
}

fn get_fast_result_return_type(ty: impl ToTokens) -> TokenStream2 {
  if tokens(&ty).contains("Result < u32") || is_resource_id(&ty) {
    quote! { -> u32 }
  } else if tokens(&ty).contains("Result < i32") {
    quote! { -> i32 }
  } else if tokens(&ty).contains("Result < f32") {
    quote! { -> f32 }
  } else if tokens(&ty).contains("Result < f64") {
    quote! { -> f64 }
  } else if tokens(&ty).contains("Result < bool") {
    quote! { -> bool }
  } else if tokens(&ty).contains("Result < ()") {
    quote! {}
  } else {
    unreachable!()
  }
}

fn is_fast_scalar(
  core: &TokenStream2,
  ty: impl ToTokens,
  is_ret: bool,
) -> Option<TokenStream2> {
  let cty = if is_ret {
    quote! { CType }
  } else {
    quote! { Type }
  };
  if is_resource_id(&ty) {
    return Some(quote! { #core::v8::fast_api::#cty::Uint32 });
  }
  if is_void(&ty) {
    return Some(quote! { #core::v8::fast_api::#cty::Void });
  }
  // TODO(@littledivy): Support u8, i8, u16, i16 by casting.
  match tokens(&ty).as_str() {
    "u32" => Some(quote! { #core::v8::fast_api::#cty::Uint32 }),
    "i32" => Some(quote! { #core::v8::fast_api::#cty::Int32 }),
    "u64" => {
      if is_ret {
        None
      } else {
        Some(quote! { #core::v8::fast_api::#cty::Uint64 })
      }
    }
    "i64" => {
      if is_ret {
        None
      } else {
        Some(quote! { #core::v8::fast_api::#cty::Int64 })
      }
    }
    // TODO(@aapoalas): Support 32 bit machines
    "usize" => {
      if is_ret {
        None
      } else {
        Some(quote! { #core::v8::fast_api::#cty::Uint64 })
      }
    }
    "isize" => {
      if is_ret {
        None
      } else {
        Some(quote! { #core::v8::fast_api::#cty::Int64 })
      }
    }
    "f32" => Some(quote! { #core::v8::fast_api::#cty::Float32 }),
    "f64" => Some(quote! { #core::v8::fast_api::#cty::Float64 }),
    "bool" => Some(quote! { #core::v8::fast_api::#cty::Bool }),
    _ => None,
  }
}

fn codegen_args(
  core: &TokenStream2,
  f: &syn::ItemFn,
  rust_i0: usize, // Index of first generic arg in rust
  v8_i0: usize,   // Index of first generic arg in v8/js
) -> (TokenStream2, TokenStream2) {
  let inputs = &f.sig.inputs.iter().skip(rust_i0).enumerate();
  let ident_seq: TokenStream2 = inputs
    .clone()
    .map(|(i, _)| format!("arg_{i}"))
    .collect::<Vec<_>>()
    .join(", ")
    .parse()
    .unwrap();
  let decls: TokenStream2 = inputs
    .clone()
    .map(|(i, arg)| {
      codegen_arg(core, arg, format!("arg_{i}").as_ref(), v8_i0 + i)
    })
    .collect();
  (decls, ident_seq)
}

fn codegen_arg(
  core: &TokenStream2,
  arg: &syn::FnArg,
  name: &str,
  idx: usize,
) -> TokenStream2 {
  let ident = quote::format_ident!("{name}");
  let (pat, ty) = match arg {
    syn::FnArg::Typed(pat) => {
      if is_optional_fast_callback_option(&pat.ty) {
        return quote! { let #ident = None; };
      }
      (&pat.pat, &pat.ty)
    }
    _ => unreachable!(),
  };
  // Fast path if arg should be skipped
  if matches!(**pat, syn::Pat::Wild(_)) {
    return quote! { let #ident = (); };
  }
  // Fast path for `String`
  if is_string(&**ty) {
    return quote! {
      let #ident = match #core::v8::Local::<#core::v8::String>::try_from(args.get(#idx as i32)) {
        Ok(v8_string) => #core::serde_v8::to_utf8(v8_string, scope),
        Err(_) => {
          return #core::_ops::throw_type_error(scope, format!("Expected string at position {}", #idx));
        }
      };
    };
  }
  // Fast path for `Option<String>`
  if is_option_string(&**ty) {
    return quote! {
      let #ident = match #core::v8::Local::<#core::v8::String>::try_from(args.get(#idx as i32)) {
        Ok(v8_string) => Some(#core::serde_v8::to_utf8(v8_string, scope)),
        Err(_) => None
      };
    };
  }
  // Fast path for &/&mut [u8] and &/&mut [u32]
  match is_ref_slice(&**ty) {
    None => {}
    Some(SliceType::U32Mut) => {
      let blck = codegen_u32_mut_slice(core, idx);
      return quote! {
        let #ident = #blck;
      };
    }
    Some(_) => {
      let blck = codegen_u8_slice(core, idx);
      return quote! {
        let #ident = #blck;
      };
    }
  }
  // Otherwise deserialize it via serde_v8
  quote! {
    let #ident = args.get(#idx as i32);
    let #ident = match #core::serde_v8::from_v8(scope, #ident) {
      Ok(v) => v,
      Err(err) => {
        let msg = format!("Error parsing args at position {}: {}", #idx, #core::anyhow::Error::from(err));
        return #core::_ops::throw_type_error(scope, msg);
      }
    };
  }
}

fn codegen_u8_slice(core: &TokenStream2, idx: usize) -> TokenStream2 {
  quote! {{
    let value = args.get(#idx as i32);
    match #core::v8::Local::<#core::v8::ArrayBuffer>::try_from(value) {
      Ok(b) => {
        let store = b.data() as *mut u8;
        // SAFETY: rust guarantees that lifetime of slice is no longer than the call.
        unsafe { ::std::slice::from_raw_parts_mut(store, b.byte_length()) }
      },
      Err(_) => {
        if let Ok(view) = #core::v8::Local::<#core::v8::ArrayBufferView>::try_from(value) {
          let (offset, len) = (view.byte_offset(), view.byte_length());
          let buffer = match view.buffer(scope) {
              Some(v) => v,
              None => {
                return #core::_ops::throw_type_error(scope, format!("Expected ArrayBufferView at position {}", #idx));
              }
          };
          let store = buffer.data() as *mut u8;
          // SAFETY: rust guarantees that lifetime of slice is no longer than the call.
          unsafe { ::std::slice::from_raw_parts_mut(store.add(offset), len) }
        } else {
          return #core::_ops::throw_type_error(scope, format!("Expected ArrayBufferView at position {}", #idx));
        }
      }
    }}
  }
}

fn codegen_u32_mut_slice(core: &TokenStream2, idx: usize) -> TokenStream2 {
  quote! {
    if let Ok(view) = #core::v8::Local::<#core::v8::Uint32Array>::try_from(args.get(#idx as i32)) {
      let (offset, len) = (view.byte_offset(), view.byte_length());
      let buffer = match view.buffer(scope) {
          Some(v) => v,
          None => {
            return #core::_ops::throw_type_error(scope, format!("Expected Uint32Array at position {}", #idx));
          }
      };
      let store = buffer.data() as *mut u8;
      // SAFETY: buffer from Uint32Array. Rust guarantees that lifetime of slice is no longer than the call.
      unsafe { ::std::slice::from_raw_parts_mut(store.add(offset) as *mut u32, len / 4) }
    } else {
      return #core::_ops::throw_type_error(scope, format!("Expected Uint32Array at position {}", #idx));
    }
  }
}

fn codegen_sync_ret(
  core: &TokenStream2,
  output: &syn::ReturnType,
) -> TokenStream2 {
  if is_void(output) {
    return quote! {};
  }

  if is_u32_rv(output) {
    return quote! {
      rv.set_uint32(result as u32);
    };
  }

  // Optimize Result<(), Err> to skip serde_v8 when Ok(...)
  let ok_block = if is_unit_result(output) {
    quote! {}
  } else if is_u32_rv_result(output) {
    quote! {
      rv.set_uint32(result as u32);
    }
  } else {
    quote! {
      match #core::serde_v8::to_v8(scope, result) {
        Ok(ret) => rv.set(ret),
        Err(err) => #core::_ops::throw_type_error(
          scope,
          format!("Error serializing return: {}", #core::anyhow::Error::from(err)),
        ),
      };
    }
  };

  if !is_result(output) {
    return ok_block;
  }

  quote! {
    match result {
      Ok(result) => {
        #ok_block
      },
      Err(err) => {
        let exception = #core::error::to_v8_error(scope, op_state.get_error_class_fn, &err);
        scope.throw_exception(exception);
      },
    };
  }
}

fn is_void(ty: impl ToTokens) -> bool {
  tokens(ty).is_empty()
}

fn is_result(ty: impl ToTokens) -> bool {
  let tokens = tokens(ty);
  if tokens.trim_start_matches("-> ").starts_with("Result <") {
    return true;
  }
  // Detect `io::Result<...>`, `anyhow::Result<...>`, etc...
  // i.e: Result aliases/shorthands which are unfortunately "opaque" at macro-time
  match tokens.find(":: Result <") {
    Some(idx) => !tokens.split_at(idx).0.contains('<'),
    None => false,
  }
}

fn is_string(ty: impl ToTokens) -> bool {
  tokens(ty) == "String"
}

fn is_option_string(ty: impl ToTokens) -> bool {
  tokens(ty) == "Option < String >"
}

enum SliceType {
  U8,
  U8Mut,
  U32Mut,
}

fn is_ref_slice(ty: impl ToTokens) -> Option<SliceType> {
  if is_u8_slice(&ty) {
    return Some(SliceType::U8);
  }
  if is_u8_slice_mut(&ty) {
    return Some(SliceType::U8Mut);
  }
  if is_u32_slice_mut(&ty) {
    return Some(SliceType::U32Mut);
  }
  None
}

fn is_u8_slice(ty: impl ToTokens) -> bool {
  tokens(ty) == "& [u8]"
}

fn is_u8_slice_mut(ty: impl ToTokens) -> bool {
  tokens(ty) == "& mut [u8]"
}

fn is_u32_slice_mut(ty: impl ToTokens) -> bool {
  tokens(ty) == "& mut [u32]"
}

fn is_optional_fast_callback_option(ty: impl ToTokens) -> bool {
  tokens(&ty).contains("Option < & mut FastApiCallbackOptions")
}

/// Detects if the type can be set using `rv.set_uint32` fast path
fn is_u32_rv(ty: impl ToTokens) -> bool {
  ["u32", "u8", "u16"].iter().any(|&s| tokens(&ty) == s) || is_resource_id(&ty)
}

/// Detects if the type is of the format Result<u32/u8/u16, Err>
fn is_u32_rv_result(ty: impl ToTokens) -> bool {
  is_result(&ty)
    && (tokens(&ty).contains("Result < u32")
      || tokens(&ty).contains("Result < u8")
      || tokens(&ty).contains("Result < u16")
      || is_resource_id(&ty))
}

/// Detects if a type is of the form Result<(), Err>
fn is_unit_result(ty: impl ToTokens) -> bool {
  is_result(&ty) && tokens(&ty).contains("Result < ()")
}

fn is_resource_id(arg: impl ToTokens) -> bool {
  static RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#": (?:deno_core :: )?ResourceId$"#).unwrap());
  RE.is_match(&tokens(arg))
}

fn is_mut_ref_opstate(arg: impl ToTokens) -> bool {
  static RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#": & mut (?:deno_core :: )?OpState$"#).unwrap());
  RE.is_match(&tokens(arg))
}

fn is_rc_refcell_opstate(arg: &syn::FnArg) -> bool {
  static RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#": Rc < RefCell < (?:deno_core :: )?OpState > >$"#).unwrap()
  });
  RE.is_match(&tokens(arg))
}

fn is_handle_scope(arg: &syn::FnArg) -> bool {
  static RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#": & mut (?:deno_core :: )?v8 :: HandleScope(?: < '\w+ >)?$"#)
      .unwrap()
  });
  RE.is_match(&tokens(arg))
}

fn is_future(ty: impl ToTokens) -> bool {
  tokens(&ty).contains("impl Future < Output =")
}

fn tokens(x: impl ToTokens) -> String {
  x.to_token_stream().to_string()
}

fn exclude_lifetime_params(
  generic_params: &Punctuated<GenericParam, Comma>,
) -> Punctuated<GenericParam, Comma> {
  generic_params
    .iter()
    .filter(|t| !tokens(t).starts_with('\''))
    .cloned()
    .collect::<Punctuated<GenericParam, Comma>>()
}
