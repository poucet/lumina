use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::parse::{HttpMethod, ParsedMethod, ParsedTrait, RestEndpoint, ReturnKind, RpcKind};

/// Generate a `RemoteXxxApi` struct that implements the trait via `RpcConnection`.
pub fn generate(parsed: &ParsedTrait) -> syn::Result<TokenStream> {
    let trait_name = &parsed.trait_name;
    let remote_name = parsed.remote_name();
    let vis = &parsed.vis;

    let method_impls: Vec<TokenStream> = parsed
        .methods
        .iter()
        .map(|m| generate_client_method(m))
        .collect::<syn::Result<Vec<_>>>()?;

    Ok(quote! {
        /// Auto-generated remote client for this API trait.
        ///
        /// Holds an `Arc<dyn RpcConnection>` and implements the trait by
        /// delegating to REST/WS calls over the connection.
        #vis struct #remote_name(::std::sync::Arc<dyn ::simply_rpc::RpcConnection>);

        impl #remote_name {
            /// Create a new remote client from an RPC connection.
            pub fn new(conn: ::std::sync::Arc<dyn ::simply_rpc::RpcConnection>) -> Self {
                Self(conn)
            }
        }

        #[::async_trait::async_trait]
        impl #trait_name for #remote_name {
            #(#method_impls)*
        }
    })
}

/// Generate a single method implementation for the remote client.
fn generate_client_method(method: &ParsedMethod) -> syn::Result<TokenStream> {
    let sig = &method.sig;
    let fn_name = &sig.ident;
    let inputs = &sig.inputs;
    let output = &sig.output;

    if method.rpc_kind == RpcKind::Skip {
        return Ok(quote! {
            async fn #fn_name(#inputs) #output {
                ::anyhow::bail!(concat!(stringify!(#fn_name), " is not available over RPC"))
            }
        });
    }

    // REST-annotated methods → generate rest_call with path interpolation
    if let Some(endpoint) = &method.rest_endpoint {
        if endpoint.http_method != HttpMethod::Stream {
            let body = generate_rest_client_body(method, endpoint);
            return Ok(quote! {
                async fn #fn_name(#inputs) #output {
                    #body
                }
            });
        }

        // StreamBidi with a path → generate bidi WS connection
        let ctx_expr = if method.has_context() {
            quote! { &ctx }
        } else {
            quote! { &::simply_rpc::RequestContext::default() }
        };
        if let ReturnKind::StreamBidi { input_type, output_type } = &method.return_kind {
            let method_str = &method.method_name;
            // URL construction uses just the path — any `?{foo}&{bar}` suffix
            // is documentation; real query params are serialised by the
            // transport from the remaining call args.
            let path_template = &endpoint.match_path;

            let path_expr = if endpoint.path_params.is_empty() {
                quote! { #path_template.to_string() }
            } else {
                let mut format_str = path_template.clone();
                let mut format_args = Vec::new();
                for param_name in &endpoint.path_params {
                    let placeholder = format!("{{{param_name}}}");
                    format_str = format_str.replace(&placeholder, "{}");
                    let param_ident = format_ident!("{}", param_name);
                    format_args.push(quote! { #param_ident });
                }
                quote! { format!(#format_str, #(#format_args),*) }
            };

            let (serialize, rpc_params) = generate_serialize(method);

            return Ok(quote! {
                async fn #fn_name(#inputs) #output {
                    #serialize
                    self.0.rpc_call(#method_str, #rpc_params, #ctx_expr).await?;
                    let (__raw_tx, mut __raw_rx) = self.0.register_stream(#method_str).await?;

                    // Wrap raw channels with typed serialization
                    let (__typed_tx, mut __typed_rx) = ::tokio::sync::mpsc::channel::<#input_type>(64);
                    let (output_tx, output_rx) = ::tokio::sync::mpsc::channel::<#output_type>(64);

                    // T → serialize → raw_tx
                    ::tokio::spawn(async move {
                        while let Some(msg) = __typed_rx.recv().await {
                            let value = ::serde_json::to_value(&msg).unwrap_or_default();
                            if __raw_tx.send(value).await.is_err() { break; }
                        }
                    });

                    // raw_rx → deserialize → U
                    ::tokio::spawn(async move {
                        while let Some(value) = __raw_rx.recv().await {
                            match ::serde_json::from_value::<#output_type>(value) {
                                Ok(event) => { if output_tx.send(event).await.is_err() { break; } }
                                Err(e) => { ::tracing::warn!("bidi stream deserialize error: {}", e); }
                            }
                        }
                    });

                    Ok(::simply_rpc::StreamHandle::new(__typed_tx, output_rx))
                }
            });
        }
    }

    // Stream or unannotated methods → WS rpc_call
    let method_str = &method.method_name;
    let (serialize, rpc_params) = generate_serialize(method);
    let body = generate_client_body(method, method_str, &serialize, &rpc_params);

    Ok(quote! {
        async fn #fn_name(#inputs) #output {
            #body
        }
    })
}

/// Generate the body for a REST-annotated client method.
fn generate_rest_client_body(method: &ParsedMethod, endpoint: &RestEndpoint) -> TokenStream {
    let http_method = match endpoint.http_method {
        HttpMethod::Get => quote! { ::simply_rpc::HttpMethod::Get },
        HttpMethod::Post => quote! { ::simply_rpc::HttpMethod::Post },
        HttpMethod::Put => quote! { ::simply_rpc::HttpMethod::Put },
        HttpMethod::Delete => quote! { ::simply_rpc::HttpMethod::Delete },
        HttpMethod::Stream => unreachable!("stream methods handled separately"),
    };

    // Path-only template — any `?{foo}&{bar}` suffix is stripped at parse
    // time and the transport serialises non-path params into the query
    // string for GET/HEAD/DELETE.
    let path_template = &endpoint.match_path;
    let path_expr = if endpoint.path_params.is_empty() {
        quote! { #path_template.to_string() }
    } else {
        let mut format_str = path_template.clone();
        let mut format_args = Vec::new();
        for param_name in &endpoint.path_params {
            let placeholder = format!("{{{param_name}}}");
            format_str = format_str.replace(&placeholder, "{}");
            let param_ident = format_ident!("{}", param_name);
            format_args.push(quote! { #param_ident });
        }
        quote! { format!(#format_str, #(#format_args),*) }
    };

    let body_params: Vec<_> = method.params.iter().filter(|p| {
        !p.is_context && !endpoint.path_params.contains(&p.name.to_string())
    }).collect();

    let body_expr = if body_params.is_empty() {
        quote! { ::serde_json::Value::Null }
    } else if body_params.len() == 1 && endpoint.path_params.is_empty() {
        let p = &body_params[0];
        let name = &p.name;
        quote! { ::serde_json::to_value(#name)? }
    } else {
        let struct_name = format_ident!("__RestClientParams_{}", method.name);
        let fields: Vec<TokenStream> = body_params.iter().map(|p| {
            let name = &p.name;
            let owned_type = &p.owned_type;
            quote! { #name: #owned_type }
        }).collect();
        let field_inits: Vec<TokenStream> = body_params.iter().map(|p| {
            let name = &p.name;
            if p.is_str_ref {
                quote! { #name: #name.to_string() }
            } else if p.is_ref {
                quote! { #name: #name.clone() }
            } else {
                quote! { #name }
            }
        }).collect();
        quote! {
            {
                #[derive(::serde::Serialize)]
                #[allow(non_camel_case_types)]
                struct #struct_name { #(#fields,)* }
                ::serde_json::to_value(&#struct_name { #(#field_inits,)* })?
            }
        }
    };

    // If method has ctx, use it; otherwise create a default
    let ctx_expr = if method.has_context() {
        quote! { &ctx }
    } else {
        quote! { &::simply_rpc::RequestContext::default() }
    };

    match &method.return_kind {
        ReturnKind::ResultUnit => quote! {
            let __path = #path_expr;
            self.0.rest_call(#http_method, &__path, #body_expr, #ctx_expr).await?;
            Ok(())
        },
        ReturnKind::ResultValue { .. } => quote! {
            let __path = #path_expr;
            let __r = self.0.rest_call(#http_method, &__path, #body_expr, #ctx_expr).await?;
            Ok(::serde_json::from_value(__r)?)
        },
        ReturnKind::RawValue { .. } => quote! {
            let __path = #path_expr;
            self.0.rest_call(#http_method, &__path, #body_expr, #ctx_expr)
                .await
                .and_then(|r| ::serde_json::from_value(r).map_err(Into::into))
                .unwrap_or_default()
        },
        _ => unreachable!("stream return types handled separately"),
    }
}

/// Generate serialization code for the client call.
fn generate_serialize(method: &ParsedMethod) -> (TokenStream, TokenStream) {
    let params = method.wire_params();

    if params.is_empty() {
        return (quote! {}, quote! { ::serde_json::Value::Null });
    }

    if params.len() == 1 {
        let p = &params[0];
        let name = &p.name;
        return (quote! {}, quote! { ::serde_json::to_value(#name)? });
    }

    let struct_name = format_ident!("__RpcClientParams_{}", method.name);
    let fields: Vec<TokenStream> = params
        .iter()
        .map(|p| {
            let name = &p.name;
            let owned_type = &p.owned_type;
            quote! { #name: #owned_type }
        })
        .collect();

    let field_inits: Vec<TokenStream> = params
        .iter()
        .filter(|p| !p.is_context)
        .map(|p| {
            let name = &p.name;
            if p.is_str_ref {
                quote! { #name: #name.to_string() }
            } else if p.is_ref {
                quote! { #name: #name.clone() }
            } else {
                quote! { #name }
            }
        })
        .collect();

    let serialize = quote! {
        #[derive(::serde::Serialize)]
        struct #struct_name {
            #(#fields,)*
        }
    };

    let params_expr = quote! {
        ::serde_json::to_value(&#struct_name {
            #(#field_inits,)*
        })?
    };

    (serialize, params_expr)
}

/// Generate the body of a client method (WS RPC path).
fn generate_client_body(
    method: &ParsedMethod,
    method_str: &str,
    serialize: &TokenStream,
    rpc_params: &TokenStream,
) -> TokenStream {
    let ctx_expr = if method.has_context() {
        quote! { &ctx }
    } else {
        quote! { &::simply_rpc::RequestContext::default() }
    };

    match &method.return_kind {
        ReturnKind::ResultUnit => {
            quote! {
                #serialize
                self.0.rpc_call(#method_str, #rpc_params, #ctx_expr).await?;
                Ok(())
            }
        }
        ReturnKind::ResultValue { .. } => {
            quote! {
                #serialize
                let __r = self.0.rpc_call(#method_str, #rpc_params, #ctx_expr).await?;
                Ok(::serde_json::from_value(__r)?)
            }
        }
        ReturnKind::RawValue { .. } => {
            let params_for_raw = if method.params.is_empty() {
                quote! { ::serde_json::Value::Null }
            } else if method.params.len() == 1 {
                let name = &method.params[0].name;
                quote! { ::serde_json::to_value(#name).unwrap_or_default() }
            } else {
                quote! { #rpc_params.unwrap_or_default() }
            };
            quote! {
                #serialize
                self.0.rpc_call(#method_str, #params_for_raw, #ctx_expr)
                    .await
                    .and_then(|r| ::serde_json::from_value(r).map_err(Into::into))
                    .unwrap_or_default()
            }
        }
        ReturnKind::StreamTuple { value_type, stream_type } => {
            // Result<(T, Stream)> — RPC returns T, then register stream for events.
            // Bridge: mpsc::Receiver<Value> → deserialize → broadcast::Sender<Event> → broadcast::Receiver
            quote! {
                #serialize
                let __r = self.0.rpc_call(#method_str, #rpc_params, #ctx_expr).await?;
                let __value: #value_type = ::serde_json::from_value(__r)?;
                let (_, mut __raw_rx) = self.0.register_stream(#method_str).await?;
                let (__broadcast_tx, __broadcast_rx) = ::tokio::sync::broadcast::channel(256);
                ::tokio::spawn(async move {
                    while let Some(value) = __raw_rx.recv().await {
                        if let Ok(event) = ::serde_json::from_value(value) {
                            let _ = __broadcast_tx.send(event);
                        }
                    }
                });
                Ok((__value, __broadcast_rx))
            }
        }
        ReturnKind::StreamBare { stream_type } => {
            quote! {
                #serialize
                self.0.rpc_call(#method_str, #rpc_params, #ctx_expr).await?;
                let (_, mut __raw_rx) = self.0.register_stream(#method_str).await?;
                let (__broadcast_tx, __broadcast_rx) = ::tokio::sync::broadcast::channel(256);
                ::tokio::spawn(async move {
                    while let Some(value) = __raw_rx.recv().await {
                        if let Ok(event) = ::serde_json::from_value(value) {
                            let _ = __broadcast_tx.send(event);
                        }
                    }
                });
                Ok(__broadcast_rx)
            }
        }
        ReturnKind::StreamBidi { input_type, output_type } => {
            quote! {
                #serialize
                self.0.rpc_call(#method_str, #rpc_params, #ctx_expr).await?;
                let (__raw_tx, mut __raw_rx) = self.0.register_stream(#method_str).await?;
                let (__typed_tx, mut __typed_rx) = ::tokio::sync::mpsc::channel::<#input_type>(64);
                let (output_tx, output_rx) = ::tokio::sync::mpsc::channel::<#output_type>(64);
                ::tokio::spawn(async move {
                    while let Some(msg) = __typed_rx.recv().await {
                        let value = ::serde_json::to_value(&msg).unwrap_or_default();
                        if __raw_tx.send(value).await.is_err() { break; }
                    }
                });
                ::tokio::spawn(async move {
                    while let Some(value) = __raw_rx.recv().await {
                        match ::serde_json::from_value::<#output_type>(value) {
                            Ok(event) => { if output_tx.send(event).await.is_err() { break; } }
                            Err(e) => { ::tracing::warn!("bidi stream deserialize error: {}", e); }
                        }
                    }
                });
                Ok(::simply_rpc::StreamHandle::new(__typed_tx, output_rx))
            }
        }
    }
}
