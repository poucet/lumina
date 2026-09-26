use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{parse_macro_input, FnArg, ItemFn, ItemMod, Meta, Pat, Type};

// ===========================================================================
// #[slash_command] — standalone command from a function
// ===========================================================================

/// Derive a slash command from an async function.
///
/// ```ignore
/// #[slash_command(description = "Check if Lumina is alive")]
/// async fn ping(lx: &LuminaContext, cmd: &CommandInteraction) -> anyhow::Result<()> {
///     cmd.create_response(&lx.http, CreateInteractionResponse::Message(
///         CreateInteractionResponseMessage::new().content("Pong!"),
///     )).await?;
///     Ok(())
/// }
/// ```
#[proc_macro_attribute]
pub fn slash_command(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attrs = parse_macro_input!(attr as CommandAttrs);
    let func = parse_macro_input!(item as ItemFn);

    match generate_command(attrs, func) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

// ===========================================================================
// #[command_group] — command with subcommands from a module
// ===========================================================================

/// Derive a command group with subcommands from a module.
///
/// Functions inside the module marked with `#[sub_command]` become subcommands.
///
/// ```ignore
/// #[command_group(description = "Chat management")]
/// mod chat {
///     use super::*;
///
///     #[sub_command(description = "Create a new chat channel")]
///     pub async fn new(
///         lx: &LuminaContext,
///         cmd: &CommandInteraction,
///         #[describe("Channel name")] channel_name: Option<String>,
///     ) -> anyhow::Result<()> { ... }
///
///     #[sub_command(description = "Pause bot responses")]
///     pub async fn pause(lx: &LuminaContext, cmd: &CommandInteraction) -> anyhow::Result<()> { ... }
/// }
/// ```
#[proc_macro_attribute]
pub fn command_group(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attrs = parse_macro_input!(attr as CommandAttrs);
    let module = parse_macro_input!(item as ItemMod);

    match generate_command_group(attrs, module) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// Marker attribute for subcommands inside a `#[command_group]` module.
/// Parsed by `#[command_group]`, not used standalone.
#[proc_macro_attribute]
pub fn sub_command(_attr: TokenStream, item: TokenStream) -> TokenStream {
    // Identity — the real work is done by #[command_group]
    item
}

// ===========================================================================
// Shared types
// ===========================================================================

struct CommandAttrs {
    description: String,
    name: Option<String>,
}

impl syn::parse::Parse for CommandAttrs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let mut description = None;
        let mut name = None;

        while !input.is_empty() {
            let ident: syn::Ident = input.parse()?;
            input.parse::<syn::Token![=]>()?;
            let lit: syn::LitStr = input.parse()?;

            match ident.to_string().as_str() {
                "description" => description = Some(lit.value()),
                "name" => name = Some(lit.value()),
                other => {
                    return Err(syn::Error::new(
                        ident.span(),
                        format!("unknown attribute `{other}`"),
                    ))
                }
            }

            if !input.is_empty() {
                input.parse::<syn::Token![,]>()?;
            }
        }

        let description =
            description.ok_or_else(|| input.error("missing `description = \"...\"`"))?;

        Ok(Self { description, name })
    }
}

struct ParamInfo {
    name: String,
    description: String,
    ty: ParamType,
    required: bool,
    autocomplete: bool,
}

enum ParamType {
    String,
    Integer,
    Number,
    Boolean,
}

impl ParamType {
    fn option_type_tokens(&self) -> TokenStream2 {
        match self {
            Self::String => quote! { serenity::all::CommandOptionType::String },
            Self::Integer => quote! { serenity::all::CommandOptionType::Integer },
            Self::Number => quote! { serenity::all::CommandOptionType::Number },
            Self::Boolean => quote! { serenity::all::CommandOptionType::Boolean },
        }
    }

    fn extract_tokens(&self, name: &str) -> TokenStream2 {
        let name_str = name;
        match self {
            Self::String => quote! {
                match opt {
                    serenity::all::ResolvedOption {
                        name: #name_str,
                        value: serenity::all::ResolvedValue::String(s),
                        ..
                    } => Some(s.to_string()),
                    _ => None,
                }
            },
            Self::Integer => quote! {
                match opt {
                    serenity::all::ResolvedOption {
                        name: #name_str,
                        value: serenity::all::ResolvedValue::Integer(n),
                        ..
                    } => Some(*n),
                    _ => None,
                }
            },
            Self::Number => quote! {
                match opt {
                    serenity::all::ResolvedOption {
                        name: #name_str,
                        value: serenity::all::ResolvedValue::Number(n),
                        ..
                    } => Some(*n),
                    _ => None,
                }
            },
            Self::Boolean => quote! {
                match opt {
                    serenity::all::ResolvedOption {
                        name: #name_str,
                        value: serenity::all::ResolvedValue::Boolean(b),
                        ..
                    } => Some(*b),
                    _ => None,
                }
            },
        }
    }
}

fn resolve_type(ty: &Type) -> syn::Result<(ParamType, bool)> {
    if let Type::Path(tp) = ty {
        let seg = tp.path.segments.last().unwrap();
        if seg.ident == "Option" {
            if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
                if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                    let (pt, _) = resolve_type(inner)?;
                    return Ok((pt, false));
                }
            }
        }
        let name = seg.ident.to_string();
        let pt = match name.as_str() {
            "String" => ParamType::String,
            "str" => ParamType::String,
            "i64" | "i32" | "u64" | "u32" | "usize" | "isize" => ParamType::Integer,
            "f64" | "f32" => ParamType::Number,
            "bool" => ParamType::Boolean,
            _ => {
                return Err(syn::Error::new_spanned(
                    ty,
                    format!("unsupported parameter type `{name}` — use String, i64, f64, or bool"),
                ))
            }
        };
        Ok((pt, true))
    } else {
        Err(syn::Error::new_spanned(ty, "unsupported parameter type"))
    }
}

fn get_describe_attr(attrs: &[syn::Attribute]) -> Option<String> {
    for attr in attrs {
        if attr.path().is_ident("describe") {
            if let Meta::List(list) = &attr.meta {
                let tokens = list.tokens.to_string();
                let trimmed = tokens.trim_matches('"');
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn to_pascal_case(s: &str) -> String {
    s.split('_')
        .map(|seg| {
            let mut c = seg.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        })
        .collect()
}

fn parse_params(func: &ItemFn) -> syn::Result<Vec<ParamInfo>> {
    let mut params = Vec::new();
    for (i, arg) in func.sig.inputs.iter().enumerate() {
        if i < 2 {
            continue; // skip lx, cmd
        }
        if let FnArg::Typed(pat_type) = arg {
            let param_name = if let Pat::Ident(pi) = pat_type.pat.as_ref() {
                pi.ident.to_string()
            } else {
                return Err(syn::Error::new_spanned(&pat_type.pat, "expected identifier"));
            };

            let (param_type, required) = resolve_type(&pat_type.ty)?;
            let desc =
                get_describe_attr(&pat_type.attrs).unwrap_or_else(|| param_name.replace('_', " "));
            let autocomplete = pat_type.attrs.iter().any(|a| a.path().is_ident("autocomplete"));

            params.push(ParamInfo {
                name: param_name,
                description: desc,
                ty: param_type,
                required,
                autocomplete,
            });
        }
    }
    Ok(params)
}

fn gen_option_registrations(params: &[ParamInfo]) -> Vec<TokenStream2> {
    params
        .iter()
        .map(|p| {
            let name = &p.name;
            let desc = &p.description;
            let opt_type = p.ty.option_type_tokens();
            let required = p.required;
            let autocomplete = p.autocomplete;
            quote! {
                .add_sub_option(
                    serenity::builder::CreateCommandOption::new(#opt_type, #name, #desc)
                        .required(#required)
                        .set_autocomplete(#autocomplete)
                )
            }
        })
        .collect()
}

fn gen_top_level_option_registrations(params: &[ParamInfo]) -> Vec<TokenStream2> {
    params
        .iter()
        .map(|p| {
            let name = &p.name;
            let desc = &p.description;
            let opt_type = p.ty.option_type_tokens();
            let required = p.required;
            let autocomplete = p.autocomplete;
            quote! {
                .add_option(
                    serenity::builder::CreateCommandOption::new(#opt_type, #name, #desc)
                        .required(#required)
                        .set_autocomplete(#autocomplete)
                )
            }
        })
        .collect()
}

fn gen_arg_extractions(params: &[ParamInfo]) -> Vec<TokenStream2> {
    params
        .iter()
        .map(|p| {
            let var_name = format_ident!("{}", p.name);
            let extract = p.ty.extract_tokens(&p.name);
            if p.required {
                let err_msg = format!("missing required argument `{}`", p.name);
                quote! {
                    let #var_name = __opts.iter().find_map(|opt| { #extract })
                        .ok_or_else(|| anyhow::anyhow!(#err_msg))?;
                }
            } else {
                quote! {
                    let #var_name = __opts.iter().find_map(|opt| { #extract });
                }
            }
        })
        .collect()
}

fn gen_call_args(params: &[ParamInfo]) -> Vec<TokenStream2> {
    params
        .iter()
        .map(|p| {
            let var_name = format_ident!("{}", p.name);
            quote! { #var_name }
        })
        .collect()
}

fn strip_describe_attrs(func: &mut ItemFn) {
    for arg in func.sig.inputs.iter_mut() {
        if let FnArg::Typed(pat_type) = arg {
            pat_type.attrs.retain(|a| !a.path().is_ident("describe") && !a.path().is_ident("autocomplete"));
        }
    }
}

// ===========================================================================
// #[slash_command] codegen
// ===========================================================================

fn generate_command(attrs: CommandAttrs, func: ItemFn) -> syn::Result<TokenStream2> {
    let fn_name = &func.sig.ident;
    let cmd_name = attrs
        .name
        .unwrap_or_else(|| fn_name.to_string().replace('_', "-"));
    let description = &attrs.description;
    let struct_name = format_ident!("{}", to_pascal_case(&fn_name.to_string()));

    let params = parse_params(&func)?;
    let option_registrations = gen_top_level_option_registrations(&params);
    let arg_extractions = gen_arg_extractions(&params);
    let call_args = gen_call_args(&params);

    let mut clean_func = func.clone();
    strip_describe_attrs(&mut clean_func);

    let opts_binding = if !params.is_empty() {
        quote! { let __opts = cmd.data.options(); }
    } else {
        quote! {}
    };

    Ok(quote! {
        #clean_func

        #[derive(Default)]
        pub struct #struct_name;

        ::inventory::submit!(crate::commands::CommandInit {
            init: || Box::new(#struct_name),
        });

        #[async_trait::async_trait]
        impl crate::commands::SlashCommand for #struct_name {
            fn name(&self) -> &'static str {
                #cmd_name
            }

            fn register(&self) -> serenity::builder::CreateCommand {
                serenity::builder::CreateCommand::new(#cmd_name)
                    .description(#description)
                    #(#option_registrations)*
            }

            async fn run(
                &self,
                lx: &crate::commands::LuminaContext,
                cmd: &serenity::all::CommandInteraction,
            ) -> anyhow::Result<()> {
                #opts_binding
                #(#arg_extractions)*
                #fn_name(lx, cmd, #(#call_args),*).await
            }
        }
    })
}

// ===========================================================================
// #[command_group] codegen
// ===========================================================================

struct SubCommandInfo {
    fn_name: syn::Ident,
    sub_name: String,
    description: String,
    params: Vec<ParamInfo>,
}

fn generate_command_group(attrs: CommandAttrs, module: ItemMod) -> syn::Result<TokenStream2> {
    let mod_name = &module.ident;
    let cmd_name = attrs
        .name
        .unwrap_or_else(|| mod_name.to_string().replace('_', "-"));
    let description = &attrs.description;
    let struct_name = format_ident!("{}", to_pascal_case(&mod_name.to_string()));

    let items = module
        .content
        .as_ref()
        .map(|(_, items)| items.as_slice())
        .unwrap_or(&[]);

    // Find all #[sub_command] functions and an optional autocomplete function
    let mut sub_commands: Vec<SubCommandInfo> = Vec::new();
    let mut has_autocomplete_fn = false;
    for item in items {
        if let syn::Item::Fn(func) = item {
            if func.sig.ident == "autocomplete" {
                has_autocomplete_fn = true;
                continue;
            }
            let sub_attr = func
                .attrs
                .iter()
                .find(|a| a.path().is_ident("sub_command"));
            if let Some(attr) = sub_attr {
                let sub_attrs: CommandAttrs = attr.parse_args()?;
                let params = parse_params(func)?;
                let sub_name = sub_attrs
                    .name
                    .unwrap_or_else(|| func.sig.ident.to_string().replace('_', "-"));
                sub_commands.push(SubCommandInfo {
                    fn_name: func.sig.ident.clone(),
                    sub_name,
                    description: sub_attrs.description,
                    params,
                });
            }
        }
    }

    if sub_commands.is_empty() {
        return Err(syn::Error::new_spanned(
            &module.ident,
            "command_group requires at least one #[sub_command] function",
        ));
    }

    // Emit cleaned module (strip #[sub_command] and #[describe] attrs)
    let mut clean_module = module.clone();
    if let Some((_, ref mut items)) = clean_module.content {
        for item in items.iter_mut() {
            if let syn::Item::Fn(func) = item {
                func.attrs.retain(|a| !a.path().is_ident("sub_command"));
                strip_describe_attrs(func);
            }
        }
    }

    // Generate subcommand option definitions for register()
    let sub_options: Vec<TokenStream2> = sub_commands
        .iter()
        .map(|sc| {
            let sub_name = &sc.sub_name;
            let sub_desc = &sc.description;
            let param_opts = gen_option_registrations(&sc.params);
            quote! {
                .add_option(
                    serenity::builder::CreateCommandOption::new(
                        serenity::all::CommandOptionType::SubCommand,
                        #sub_name,
                        #sub_desc,
                    )
                    #(#param_opts)*
                )
            }
        })
        .collect();

    // Generate dispatch match arms for run()
    let match_arms: Vec<TokenStream2> = sub_commands
        .iter()
        .map(|sc| {
            let sub_name = &sc.sub_name;
            let fn_name = &sc.fn_name;
            let arg_extractions = gen_arg_extractions(&sc.params);
            let call_args = gen_call_args(&sc.params);
            quote! {
                #sub_name => {
                    if let serenity::all::ResolvedValue::SubCommand(ref __opts) = __sub.value {
                        #(#arg_extractions)*
                        #mod_name::#fn_name(lx, cmd, #(#call_args),*).await
                    } else {
                        Err(anyhow::anyhow!("expected subcommand value"))
                    }
                }
            }
        })
        .collect();

    let autocomplete_impl = if has_autocomplete_fn {
        quote! {
            async fn autocomplete(
                &self,
                lx: &crate::commands::LuminaContext,
                cmd: &serenity::all::CommandInteraction,
            ) -> anyhow::Result<()> {
                #mod_name::autocomplete(lx, cmd).await
            }
        }
    } else {
        quote! {}
    };

    Ok(quote! {
        #clean_module

        #[derive(Default)]
        pub struct #struct_name;

        ::inventory::submit!(crate::commands::CommandInit {
            init: || Box::new(#struct_name),
        });

        #[async_trait::async_trait]
        impl crate::commands::SlashCommand for #struct_name {
            fn name(&self) -> &'static str {
                #cmd_name
            }

            fn register(&self) -> serenity::builder::CreateCommand {
                serenity::builder::CreateCommand::new(#cmd_name)
                    .description(#description)
                    #(#sub_options)*
            }

            async fn run(
                &self,
                lx: &crate::commands::LuminaContext,
                cmd: &serenity::all::CommandInteraction,
            ) -> anyhow::Result<()> {
                let __opts = cmd.data.options();
                let __sub = __opts.first()
                    .ok_or_else(|| anyhow::anyhow!("missing subcommand"))?;
                match __sub.name {
                    #(#match_arms)*
                    other => Err(anyhow::anyhow!("unknown subcommand `{other}`")),
                }
            }

            #autocomplete_impl
        }
    })
}
