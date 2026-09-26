use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::parse::{HttpMethod, ParsedMethod, ParsedTrait, ReturnKind, RpcKind};

/// Convert a parse-time HttpMethod to the runtime RouteKind token.
fn route_kind_tokens(method: &HttpMethod) -> TokenStream {
    match method {
        HttpMethod::Get => quote! { ::simply_rpc::RouteKind::Rest(::simply_rpc::HttpMethod::Get) },
        HttpMethod::Post => quote! { ::simply_rpc::RouteKind::Rest(::simply_rpc::HttpMethod::Post) },
        HttpMethod::Put => quote! { ::simply_rpc::RouteKind::Rest(::simply_rpc::HttpMethod::Put) },
        HttpMethod::Delete => quote! { ::simply_rpc::RouteKind::Rest(::simply_rpc::HttpMethod::Delete) },
        HttpMethod::Stream => quote! { ::simply_rpc::RouteKind::Stream },
    }
}

/// Check if a return type is `BinaryResponse` (or `Result<BinaryResponse>`).
fn is_binary_response_type(return_kind: &ReturnKind) -> bool {
    match return_kind {
        ReturnKind::ResultValue { inner } | ReturnKind::RawValue { inner } => {
            let ty_str = quote! { #inner }.to_string().replace(' ', "");
            ty_str == "BinaryResponse" || ty_str.ends_with("::BinaryResponse")
        }
        _ => false,
    }
}

/// Check if any parameter is `BinaryUpload`.
fn has_binary_upload_param(method: &ParsedMethod) -> bool {
    method.params.iter().any(|p| {
        let ty = &p.owned_type;
        let ty_str = quote! { #ty }.to_string().replace(' ', "");
        ty_str == "BinaryUpload" || ty_str.ends_with("::BinaryUpload")
    })
}

/// Generate the `XxxApiService<T>` struct + `RpcService` impl + metadata constant.
pub fn generate(parsed: &ParsedTrait) -> syn::Result<TokenStream> {
    let service_name = parsed.service_name();
    let trait_name = &parsed.trait_name;
    let prefix = &parsed.prefix;
    let vis = &parsed.vis;
    let meta_const_name = parsed.meta_const_name();

    let rpc_methods: Vec<_> = parsed
        .methods
        .iter()
        .filter(|m| m.rpc_kind != RpcKind::Skip)
        .filter(|m| !matches!(m.return_kind, ReturnKind::StreamBidi { .. }))
        .collect();

    let match_arms: Vec<TokenStream> = rpc_methods
        .iter()
        .map(|m| generate_dispatch_arm(m))
        .collect::<syn::Result<Vec<_>>>()?;

    let stream_type = find_stream_type(&rpc_methods);

    // Generate method metadata entries
    let method_metas: Vec<TokenStream> = rpc_methods
        .iter()
        .map(|m| {
            let name = &m.method_name;
            let hash = signature_hash(m);
            quote! {
                ::simply_rpc::MethodMeta {
                    name: #name,
                    signature_hash: #hash,
                }
            }
        })
        .collect();

    // Generate tool schema functions + route metadata entries
    let mut schema_fns: Vec<TokenStream> = Vec::new();
    let route_metas: Vec<TokenStream> = parsed
        .methods
        .iter()
        .filter_map(|m| {
            let endpoint = m.rest_endpoint.as_ref()?;
            let kind = route_kind_tokens(&endpoint.http_method);
            // matchit only understands the path portion — the optional
            // `?{foo}&{bar}` suffix is documentation stripped at parse time.
            let path = &endpoint.match_path;
            let method_name = &m.method_name;
            let description = match &m.doc_comment {
                Some(doc) => quote! { Some(#doc) },
                None => quote! { None },
            };
            let no_tool = m.no_tool;
            let immutable_cache = m.immutable_cache;
            let binary_response = is_binary_response_type(&m.return_kind);
            let binary_upload = has_binary_upload_param(m);

            // Generate tool_schema function (exclude RequestContext and BinaryUpload)
            let tool_params: Vec<_> = m.params.iter()
                .filter(|p| {
                    if p.is_context { return false; }
                    let ty = &p.owned_type;
                    let ty_str = quote! { #ty }.to_string().replace(' ', "");
                    ty_str != "BinaryUpload" && !ty_str.ends_with("::BinaryUpload")
                })
                .collect();

            let schema_fn_name = format_ident!("__tool_schema_{}", m.name);
            let schema_mod = format_ident!("__tool_schemas_{}", prefix);
            let tool_schema = if tool_params.is_empty() || no_tool {
                schema_fns.push(quote! {
                    pub fn #schema_fn_name() -> ::core::option::Option<&'static ::schemars::Schema> { None }
                });
                quote! { #schema_mod::#schema_fn_name }
            } else {
                let fields: Vec<TokenStream> = tool_params.iter().map(|p| {
                    let name = &p.name;
                    let ty = &p.owned_type;
                    quote! { pub #name: #ty }
                }).collect();

                schema_fns.push(quote! {
                    pub fn #schema_fn_name() -> ::core::option::Option<&'static ::schemars::Schema> {
                        #[derive(::schemars::JsonSchema)]
                        #[allow(non_camel_case_types)]
                        struct Params {
                            #(#fields),*
                        }
                        static SCHEMA: ::std::sync::LazyLock<::schemars::Schema> =
                            ::std::sync::LazyLock::new(|| ::schemars::schema_for!(Params));
                        Some(&SCHEMA)
                    }
                });
                quote! { #schema_mod::#schema_fn_name }
            };

            Some(quote! {
                ::simply_rpc::RouteMeta {
                    kind: #kind,
                    path_template: #path,
                    method_name: #method_name,
                    description: #description,
                    no_tool: #no_tool,
                    binary_response: #binary_response,
                    binary_upload: #binary_upload,
                    immutable_cache: #immutable_cache,
                    tool_schema: #tool_schema,
                }
            })
        })
        .collect();

    // Generate rest_dispatch match arms
    let rest_dispatch_arms = generate_rest_dispatch_arms(parsed);

    // Generate ws_dispatch match arms for stream methods
    let ws_dispatch_arms = generate_ws_dispatch_arms(parsed);

    let schema_mod_name = format_ident!("__tool_schemas_{}", prefix);

    Ok(quote! {
        #[doc(hidden)]
        #[allow(non_snake_case)]
        mod #schema_mod_name {
            use super::*;
            #(#schema_fns)*
        }

        /// Auto-generated RPC service wrapper (internal — use `TraitName::service()` instead).
        #[doc(hidden)]
        #vis struct #service_name<T: #trait_name + ?Sized>(::std::sync::Arc<T>);

        /// Auto-generated method metadata for compatibility checking.
        #vis const #meta_const_name: ::simply_rpc::ServiceMeta = ::simply_rpc::ServiceMeta {
            prefix: #prefix,
            methods: &[#(#method_metas),*],
            routes: &[#(#route_metas),*],
        };

        impl dyn #trait_name {
            /// Create an RPC service from an `Arc<dyn Trait>`.
            pub fn service(arc: ::std::sync::Arc<dyn #trait_name>) -> ::std::sync::Arc<#service_name<dyn #trait_name>> {
                ::std::sync::Arc::new(#service_name(arc))
            }

            /// Get the service metadata for compatibility checking.
            pub fn meta() -> &'static ::simply_rpc::ServiceMeta {
                &#meta_const_name
            }
        }

        #[::async_trait::async_trait]
        impl<T: #trait_name + ?Sized + 'static> ::simply_rpc::RestService for #service_name<T> {
            async fn rest_dispatch_by_name(
                &self,
                method_name: &str,
                ctx: &::simply_rpc::RequestContext,
                params: ::serde_json::Value,
            ) -> Option<::simply_rpc::RpcResult> {
                #rest_dispatch_arms
            }

            async fn ws_dispatch_by_name(
                &self,
                method_name: &str,
                ctx: &::simply_rpc::RequestContext,
                params: ::serde_json::Value,
                write_tx: ::tokio::sync::mpsc::Sender<String>,
            ) -> Option<::simply_rpc::WsDispatchResult> {
                #ws_dispatch_arms
            }

            fn meta(&self) -> &'static ::simply_rpc::ServiceMeta {
                &#meta_const_name
            }
        }

        #[::async_trait::async_trait]
        impl<T: #trait_name + ?Sized + 'static> ::simply_rpc::RpcService for #service_name<T> {
            type Stream = #stream_type;

            fn prefix(&self) -> &str {
                #prefix
            }

            fn meta(&self) -> &'static ::simply_rpc::ServiceMeta {
                &#meta_const_name
            }

            async fn dispatch(
                &self,
                method: &str,
                ctx: &::simply_rpc::RequestContext,
                params: ::serde_json::Value,
            ) -> Option<::simply_rpc::DispatchResult<Self::Stream>> {
                let _ = ctx; // suppress unused if no method uses ctx
                match method {
                    #(#match_arms)*
                    _ => None,
                }
            }
        }
    })
}

/// Hash a method's full signature (params + return) for compatibility checking.
fn signature_hash(method: &ParsedMethod) -> u64 {
    let mut hasher = DefaultHasher::new();
    // Hash param types (owned forms, since that's what goes over the wire)
    for p in &method.params {
        let owned = &p.owned_type;
        let ty_str = quote! { #owned }.to_string();
        ty_str.hash(&mut hasher);
    }
    // Hash return kind + type
    let ret_str = match &method.return_kind {
        ReturnKind::ResultUnit => "Result<()>".to_string(),
        ReturnKind::ResultValue { inner } => format!("Result<{}>", quote! { #inner }),
        ReturnKind::RawValue { inner } => quote! { #inner }.to_string(),
        ReturnKind::StreamTuple { value_type, stream_type } => {
            format!("Result<({}, {})>", quote! { #value_type }, quote! { #stream_type })
        }
        ReturnKind::StreamBare { stream_type } => {
            format!("Result<{}>", quote! { #stream_type })
        }
        ReturnKind::StreamBidi { input_type, output_type } => {
            format!("Result<StreamHandle<{}, {}>>", quote! { #input_type }, quote! { #output_type })
        }
    };
    ret_str.hash(&mut hasher);
    hasher.finish()
}

/// Find the stream type from stream methods, or default to `()`.
fn find_stream_type(methods: &[&ParsedMethod]) -> TokenStream {
    for m in methods {
        match &m.return_kind {
            ReturnKind::StreamTuple { stream_type, .. }
            | ReturnKind::StreamBare { stream_type } => {
                return quote! { #stream_type };
            }
            ReturnKind::StreamBidi { output_type, .. } => {
                // Bidi streams produce Receiver<U> as the stream type
                return quote! { ::tokio::sync::mpsc::Receiver<#output_type> };
            }
            _ => {}
        }
    }
    quote! { () }
}

/// Generate a single match arm for a method.
fn generate_dispatch_arm(method: &ParsedMethod) -> syn::Result<TokenStream> {
    let method_str = &method.method_name;

    let (deser, call_args) = generate_deser_and_args(method);
    let body = generate_dispatch_body(method, &call_args);

    Ok(quote! {
        #method_str => {
            #deser
            #body
        }
    })
}

/// Generate deserialization code and the arguments to pass to the trait method.
/// Used by the generic `dispatch` method (not REST/WS — those handle ctx separately).
fn generate_deser_and_args(method: &ParsedMethod) -> (TokenStream, Vec<TokenStream>) {
    let wire_params = method.wire_params();
    let mut call_args = Vec::new();

    // Inject ctx from the dispatch method parameter
    if method.has_context() {
        call_args.push(quote! { ctx });
    }

    if wire_params.is_empty() {
        return (quote! {}, call_args);
    }

    if wire_params.len() == 1 {
        let p = &wire_params[0];
        let name = &p.name;
        let owned_type = &p.owned_type;

        let deser = quote! {
            let #name: #owned_type = match ::serde_json::from_value(params) {
                Ok(v) => v,
                Err(e) => return Some(::simply_rpc::DispatchResult::value(
                    Err(::anyhow::anyhow!("deserialize error: {}", e))
                )),
            };
        };

        let arg = if p.is_ref || p.is_str_ref {
            quote! { &#name }
        } else {
            quote! { #name }
        };
        call_args.push(arg);

        return (deser, call_args);
    }

    // Multi-param: generate a Params struct
    let struct_name = format_ident!("__RpcParams_{}", method.name);
    let fields: Vec<TokenStream> = wire_params
        .iter()
        .map(|p| {
            let name = &p.name;
            let owned_type = &p.owned_type;
            quote! { #name: #owned_type }
        })
        .collect();

    let deser = quote! {
        #[derive(::serde::Deserialize)]
        #[allow(non_camel_case_types)]
        struct #struct_name {
            #(#fields,)*
        }
        let __p: #struct_name = match ::serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => return Some(::simply_rpc::DispatchResult::value(
                Err(::anyhow::anyhow!("deserialize error: {}", e))
            )),
        };
    };

    for p in &wire_params {
        let name = &p.name;
        if p.is_ref || p.is_str_ref {
            call_args.push(quote! { &__p.#name });
        } else {
            call_args.push(quote! { __p.#name });
        }
    }

    (deser, call_args)
}

/// Generate the body of `rest_dispatch_by_name` — a match on method name.
///
/// The `ServiceRouter` (using matchit) handles path matching and param extraction.
/// By the time this is called, `params` is a flat JSON object with all parameters
/// (path params as strings, body params as-is).
fn generate_rest_dispatch_arms(parsed: &ParsedTrait) -> TokenStream {
    let arms: Vec<TokenStream> = parsed
        .methods
        .iter()
        .filter_map(|m| {
            let endpoint = m.rest_endpoint.as_ref()?;
            if endpoint.http_method == HttpMethod::Stream {
                return None;
            }

            let method_name = &m.method_name;
            let fn_name = &m.name;

            // Separate context params (injected) from wire params (deserialized)
            let wire_params = m.wire_params();
            let has_path_params = !endpoint.path_params.is_empty();

            let mut param_bindings = Vec::new();
            let mut call_args = Vec::new();

            // If method has a RequestContext param, inject it from `ctx`
            if m.has_context() {
                call_args.push(quote! { &ctx });
            }

            if wire_params.is_empty() {
                // No wire params — nothing to deserialize
            } else if wire_params.len() == 1 && !has_path_params {
                // Single param, no path params — params IS the value directly
                let p = &wire_params[0];
                let name = &p.name;
                let owned_type = &p.owned_type;
                param_bindings.push(quote! {
                    let #name: #owned_type = match ::serde_json::from_value(params.clone()) {
                        Ok(v) => v,
                        Err(e) => return Some(Err(::anyhow::anyhow!("deserialize error: {}", e))),
                    };
                });
                if p.is_ref || p.is_str_ref {
                    call_args.push(quote! { &#name });
                } else {
                    call_args.push(quote! { #name });
                }
            } else {
                // Multiple params or has path params — extract each from JSON object
                for p in &wire_params {
                    let name = &p.name;
                    let name_str = name.to_string();
                    let owned_type = &p.owned_type;

                    if p.is_str_ref {
                        param_bindings.push(quote! {
                            let #name: String = match params.get(#name_str) {
                                Some(v) => match ::serde_json::from_value(v.clone()) {
                                    Ok(v) => v,
                                    Err(e) => return Some(Err(::anyhow::anyhow!("deserialize '{}': {}", #name_str, e))),
                                },
                                None => return Some(Err(::anyhow::anyhow!("missing field: {}", #name_str))),
                            };
                        });
                    } else if p.is_optional {
                        // `Option<T>` params default to `None` when absent —
                        // lets GET query strings omit optional filters
                        // naturally instead of requiring the caller to
                        // always pass every field.
                        param_bindings.push(quote! {
                            let #name: #owned_type = match params.get(#name_str) {
                                Some(v) => match ::serde_json::from_value(v.clone()) {
                                    Ok(v) => v,
                                    Err(e) => return Some(Err(::anyhow::anyhow!("deserialize '{}': {}", #name_str, e))),
                                },
                                None => None,
                            };
                        });
                    } else {
                        param_bindings.push(quote! {
                            let #name: #owned_type = match params.get(#name_str) {
                                Some(v) => match ::serde_json::from_value(v.clone()) {
                                    Ok(v) => v,
                                    Err(e) => return Some(Err(::anyhow::anyhow!("deserialize '{}': {}", #name_str, e))),
                                },
                                None => return Some(Err(::anyhow::anyhow!("missing field: {}", #name_str))),
                            };
                        });
                    }

                    if p.is_ref || p.is_str_ref {
                        call_args.push(quote! { &#name });
                    } else {
                        call_args.push(quote! { #name });
                    }
                }
            }

            let call = quote! { self.0.#fn_name(#(#call_args),*).await };
            let result_wrap = match &m.return_kind {
                ReturnKind::ResultUnit => quote! {
                    match #call {
                        Ok(()) => Some(Ok(::serde_json::Value::Bool(true))),
                        Err(e) => Some(Err(e)),
                    }
                },
                ReturnKind::ResultValue { .. } => {
                    quote! { Some(::simply_rpc::call_val(#call)) }
                },
                ReturnKind::RawValue { .. } => quote! { Some(::simply_rpc::call_raw(#call)) },
                _ => return None,
            };

            Some(quote! {
                #method_name => {
                    #(#param_bindings)*
                    return #result_wrap;
                }
            })
        })
        .collect();

    if arms.is_empty() {
        quote! { let _ = (method_name, ctx, params); None }
    } else {
        quote! {
            let _ = ctx; // suppress unused if no method uses ctx
            match method_name {
                #(#arms)*
                _ => None,
            }
        }
    }
}

/// Generate `ws_dispatch_by_name` match arms for stream methods.
///
/// Each arm calls the trait method, spawns a forwarder for the output stream,
/// creates an input sink, and returns `WsDispatchResult`.
fn generate_ws_dispatch_arms(parsed: &ParsedTrait) -> TokenStream {
    let arms: Vec<TokenStream> = parsed
        .methods
        .iter()
        .filter(|m| m.rpc_kind == RpcKind::Stream)
        .filter_map(|m| {
            let method_name = &m.method_name;
            let fn_name = &m.name;
            let endpoint = m.rest_endpoint.as_ref()?;
            if endpoint.http_method != HttpMethod::Stream {
                return None;
            }

            let wire_params = m.wire_params();

            // Generate param bindings (same as REST dispatch but for WS)
            let mut param_bindings = Vec::new();
            let mut call_args = Vec::new();

            // Inject context if method needs it
            if m.has_context() {
                call_args.push(quote! { &ctx });
            }

            if wire_params.is_empty() {
                // nothing
            } else if wire_params.len() == 1 {
                // Single param — try direct deserialization first, then object field lookup
                let p = &wire_params[0];
                let name = &p.name;
                let name_str = name.to_string();
                let owned_type = &p.owned_type;
                param_bindings.push(quote! {
                    let #name: #owned_type = match ::serde_json::from_value::<#owned_type>(params.clone()) {
                        Ok(v) => v,
                        Err(_) => match params.get(#name_str).and_then(|v| ::serde_json::from_value::<#owned_type>(v.clone()).ok()) {
                            Some(v) => v,
                            None => return Some(::simply_rpc::WsDispatchResult {
                                result: Err(::anyhow::anyhow!("missing or invalid field: {}", #name_str)),
                                input_sink: { let (tx, _) = ::tokio::sync::mpsc::channel(1); tx },
                            }),
                        },
                    };
                });
                if p.is_ref || p.is_str_ref {
                    call_args.push(quote! { &#name });
                } else {
                    call_args.push(quote! { #name });
                }
            } else {
                for p in &wire_params {
                    let name = &p.name;
                    let name_str = name.to_string();
                    let owned_type = &p.owned_type;
                    param_bindings.push(quote! {
                        let #name: #owned_type = match params.get(#name_str) {
                            Some(v) => match ::serde_json::from_value(v.clone()) {
                                Ok(v) => v,
                                Err(e) => return Some(::simply_rpc::WsDispatchResult {
                                    result: Err(::anyhow::anyhow!("deserialize '{}': {}", #name_str, e)),
                                    input_sink: { let (tx, _) = ::tokio::sync::mpsc::channel(1); tx },
                                }),
                            },
                            None => return Some(::simply_rpc::WsDispatchResult {
                                result: Err(::anyhow::anyhow!("missing field: {}", #name_str)),
                                input_sink: { let (tx, _) = ::tokio::sync::mpsc::channel(1); tx },
                            }),
                        };
                    });
                    if p.is_ref || p.is_str_ref {
                        call_args.push(quote! { &#name });
                    } else {
                        call_args.push(quote! { #name });
                    }
                }
            }

            let call = quote! { self.0.#fn_name(#(#call_args),*).await };
            let method_name_event = format!("{method_name}.event");

            let body = match &m.return_kind {
                ReturnKind::StreamBidi { input_type, .. } => {
                    // StreamHandle<T, U> — split into sender + receiver
                    quote! {
                        match #call {
                            Ok(__handle) => {
                                let (__input_tx, mut __output_rx) = __handle.into_parts();

                                // Spawn output forwarder: Receiver<U> → write_tx as JSON
                                let __write = write_tx.clone();
                                let __evt_method = #method_name_event.to_string();
                                ::tokio::spawn(async move {
                                    while let Some(event) = __output_rx.recv().await {
                                        let notif = ::serde_json::json!({
                                            "method": __evt_method,
                                            "params": ::serde_json::to_value(&event).unwrap_or_default(),
                                        });
                                        if __write.send(notif.to_string()).await.is_err() { break; }
                                    }
                                });

                                // Create input sink: JSON Value → deserialize as T → input_tx
                                let (__json_tx, mut __json_rx) = ::tokio::sync::mpsc::channel::<::serde_json::Value>(64);
                                ::tokio::spawn(async move {
                                    while let Some(value) = __json_rx.recv().await {
                                        match ::serde_json::from_value::<#input_type>(value) {
                                            Ok(msg) => { if __input_tx.send(msg).await.is_err() { break; } }
                                            Err(e) => { ::tracing::warn!("stream input deserialize error: {}", e); }
                                        }
                                    }
                                });

                                Some(::simply_rpc::WsDispatchResult {
                                    result: Ok(::serde_json::Value::Bool(true)),
                                    input_sink: __json_tx,
                                })
                            }
                            Err(e) => Some(::simply_rpc::WsDispatchResult {
                                result: Err(e),
                                input_sink: { let (tx, _) = ::tokio::sync::mpsc::channel(1); tx },
                            }),
                        }
                    }
                }
                ReturnKind::StreamTuple { .. } => {
                    // (Value, broadcast::Receiver<T>) — forward receiver as events
                    quote! {
                        match #call {
                            Ok((__value, mut __stream)) => {
                                let __write = write_tx.clone();
                                let __evt_method = #method_name_event.to_string();
                                ::tokio::spawn(async move {
                                    loop {
                                        match __stream.recv().await {
                                            Ok(event) => {
                                                let notif = ::serde_json::json!({
                                                    "method": __evt_method,
                                                    "params": ::serde_json::to_value(&event).unwrap_or_default(),
                                                });
                                                if __write.send(notif.to_string()).await.is_err() { break; }
                                            }
                                            Err(::tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                                ::tracing::warn!(skipped = n, "stream forwarder lagged");
                                            }
                                            Err(::tokio::sync::broadcast::error::RecvError::Closed) => break,
                                        }
                                    }
                                });
                                let (__sink_tx, _) = ::tokio::sync::mpsc::channel(64);
                                Some(::simply_rpc::WsDispatchResult {
                                    result: ::simply_rpc::call_val(Ok(__value)),
                                    input_sink: __sink_tx,
                                })
                            }
                            Err(e) => Some(::simply_rpc::WsDispatchResult {
                                result: Err(e),
                                input_sink: { let (tx, _) = ::tokio::sync::mpsc::channel(1); tx },
                            }),
                        }
                    }
                }
                ReturnKind::StreamBare { .. } => {
                    // broadcast::Receiver<T> — forward as events
                    quote! {
                        match #call {
                            Ok(mut __stream) => {
                                let __write = write_tx.clone();
                                let __evt_method = #method_name_event.to_string();
                                ::tokio::spawn(async move {
                                    loop {
                                        match __stream.recv().await {
                                            Ok(event) => {
                                                let notif = ::serde_json::json!({
                                                    "method": __evt_method,
                                                    "params": ::serde_json::to_value(&event).unwrap_or_default(),
                                                });
                                                if __write.send(notif.to_string()).await.is_err() { break; }
                                            }
                                            Err(::tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                                ::tracing::warn!(skipped = n, "stream forwarder lagged");
                                            }
                                            Err(::tokio::sync::broadcast::error::RecvError::Closed) => break,
                                        }
                                    }
                                });
                                let (__sink_tx, _) = ::tokio::sync::mpsc::channel(64);
                                Some(::simply_rpc::WsDispatchResult {
                                    result: Ok(::serde_json::Value::Bool(true)),
                                    input_sink: __sink_tx,
                                })
                            }
                            Err(e) => Some(::simply_rpc::WsDispatchResult {
                                result: Err(e),
                                input_sink: { let (tx, _) = ::tokio::sync::mpsc::channel(1); tx },
                            }),
                        }
                    }
                }
                _ => return None,
            };

            Some(quote! {
                #method_name => {
                    #(#param_bindings)*
                    return #body;
                }
            })
        })
        .collect();

    if arms.is_empty() {
        quote! { let _ = (method_name, ctx, params, write_tx); None }
    } else {
        quote! {
            match method_name {
                #(#arms)*
                _ => None,
            }
        }
    }
}

/// Generate the body that calls the trait method and wraps the result.
fn generate_dispatch_body(method: &ParsedMethod, call_args: &[TokenStream]) -> TokenStream {
    let fn_name = &method.name;
    let call = quote! { self.0.#fn_name(#(#call_args),*).await };

    match &method.return_kind {
        ReturnKind::ResultUnit => {
            quote! {
                Some(::simply_rpc::DispatchResult::value(
                    ::simply_rpc::call_unit(#call)
                ))
            }
        }
        ReturnKind::ResultValue { .. } => {
            quote! {
                Some(::simply_rpc::DispatchResult::value(
                    ::simply_rpc::call_val(#call)
                ))
            }
        }
        ReturnKind::RawValue { .. } => {
            quote! {
                Some(::simply_rpc::DispatchResult::value(
                    ::simply_rpc::call_raw(#call)
                ))
            }
        }
        ReturnKind::StreamTuple { .. } => {
            // Result<(T, S)> — serialize T as the result, return S as a stream
            quote! {
                Some(match #call {
                    Ok((__value, __stream)) => {
                        ::simply_rpc::DispatchResult::with_stream(
                            ::simply_rpc::call_val(Ok(__value)),
                            __stream,
                        )
                    }
                    Err(e) => ::simply_rpc::DispatchResult::value(Err(e)),
                })
            }
        }
        ReturnKind::StreamBare { .. } => {
            // Result<S> — return true as the result, S as a stream
            quote! {
                Some(match #call {
                    Ok(__stream) => {
                        ::simply_rpc::DispatchResult::with_stream(
                            Ok(::serde_json::Value::Bool(true)),
                            __stream,
                        )
                    }
                    Err(e) => ::simply_rpc::DispatchResult::value(Err(e)),
                })
            }
        }
        ReturnKind::StreamBidi { input_type, output_type: _ } => {
            // Result<StreamHandle<T, U>> — create channels, return bidi stream
            // The service returns StreamHandle. We split it: the Receiver<U> goes
            // as the stream (server→client), and we create a json→T deserializer
            // channel for client→server that the WS handler feeds into.
            quote! {
                Some(match #call {
                    Ok(__handle) => {
                        let (__client_tx, __client_rx) = __handle.into_parts();
                        // Create a JSON input channel. The WS handler sends raw
                        // serde_json::Value here; we deserialize and forward.
                        let (__json_tx, mut __json_rx) = ::tokio::sync::mpsc::channel::<::serde_json::Value>(64);

                        // Spawn deserializer: JSON Value → T → client_tx
                        ::tokio::spawn(async move {
                            while let Some(value) = __json_rx.recv().await {
                                match ::serde_json::from_value::<#input_type>(value) {
                                    Ok(msg) => {
                                        if __client_tx.send(msg).await.is_err() { break; }
                                    }
                                    Err(e) => {
                                        ::tracing::warn!("bidi stream deserialize error: {}", e);
                                    }
                                }
                            }
                        });

                        ::simply_rpc::DispatchResult::with_bidi_stream(
                            Ok(::serde_json::Value::Bool(true)),
                            __client_rx,
                            __json_tx,
                        )
                    }
                    Err(e) => ::simply_rpc::DispatchResult::value(Err(e)),
                })
            }
        }
    }
}
