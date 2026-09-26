use proc_macro2::TokenStream;
use quote::{quote, format_ident, ToTokens};
use syn::{
    FnArg, ImplItem, ItemImpl, Pat, PatType, Result, ReturnType,
    Type, Ident, spanned::Spanned,
};

/// Information about a command method
struct CommandInfo {
    method_name: Ident,
    self_type: Type,
    args: Vec<ArgInfo>,
    return_type: ReturnType,
    command_name: String,
    help_text: String,
    completers: std::collections::HashMap<String, Ident>, // arg_name -> completer_method_name
}

/// Information about a method argument
struct ArgInfo {
    name: Ident,
    ty: Type,
    is_optional: bool,
    /// Inner type for Option<T> (for completion routing)
    inner_ty: Option<Type>,
}

/// Parse completer attribute to extract both command and arg names
/// Returns (command_name, arg_name) - command_name is optional for backwards compatibility
fn parse_completer_attribute(attrs: &[syn::Attribute]) -> Result<Option<(Option<String>, String)>> {
    for attr in attrs {
        if attr.path().is_ident("completer") {
            let mut command_name = None;
            let mut arg_name = None;
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("command") {
                    let val = meta.value()?;
                    let s: syn::LitStr = val.parse()?;
                    command_name = Some(s.value());
                } else if meta.path.is_ident("arg") {
                    let val = meta.value()?;
                    let s: syn::LitStr = val.parse()?;
                    arg_name = Some(s.value());
                }
                Ok(())
            })?;
            if let Some(arg) = arg_name {
                return Ok(Some((command_name, arg)));
            }
        }
    }
    Ok(None)
}

/// Parse #[command(name = "...", help = "...")] attributes
fn parse_command_attribute(attrs: &[syn::Attribute]) -> Result<Option<(String, String)>> {
    let mut command_name = None;
    let mut help_text = String::new();

    for attr in attrs {
        if attr.path().is_ident("command") {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("name") {
                    let value = meta.value()?;
                    let s: syn::LitStr = value.parse()?;
                    command_name = Some(s.value());
                } else if meta.path.is_ident("help") {
                    let value = meta.value()?;
                    let s: syn::LitStr = value.parse()?;
                    help_text = s.value();
                }
                Ok(())
            })?;
        }
    }

    Ok(command_name.map(|name| (name, help_text)))
}

/// Parse command attributes to extract name and help text from all methods
fn parse_command_attrs(impl_block: &ItemImpl) -> Result<Vec<(String, String)>> {
    impl_block.items.iter()
        .filter_map(|item| {
            if let ImplItem::Fn(method) = item {
                parse_command_attribute(&method.attrs).transpose()
            } else {
                None
            }
        })
        .collect()
}

/// Parse argument information from PatType
fn parse_arg_info(pat_type: &PatType) -> Result<ArgInfo> {
    let name = if let Pat::Ident(pat_ident) = &*pat_type.pat {
        pat_ident.ident.clone()
    } else {
        return Err(syn::Error::new(
            pat_type.pat.span(),
            "Expected simple identifier pattern",
        ));
    };

    let ty = (*pat_type.ty).clone();

    // Check if type is Option<T> and extract inner type
    let (is_optional, inner_ty) = extract_option_inner_type(&ty);

    Ok(ArgInfo {
        name,
        ty,
        is_optional,
        inner_ty,
    })
}

/// Check if a type is Option<T> and extract T
fn extract_option_inner_type(ty: &Type) -> (bool, Option<Type>) {
    if let Type::Path(type_path) = ty {
        if let Some(segment) = type_path.path.segments.last() {
            if segment.ident == "Option" {
                // Extract the T from Option<T>
                if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                    if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                        return (true, Some(inner.clone()));
                    }
                }
                return (true, None);
            }
        }
    }
    (false, None)
}

/// Get the type to use for parsing/completion (unwraps Option<T> to T)
fn get_effective_type(arg: &ArgInfo) -> &Type {
    if arg.is_optional {
        arg.inner_ty.as_ref().unwrap_or(&arg.ty)
    } else {
        &arg.ty
    }
}

/// Generate a single argument parsing statement
fn generate_arg_parse(arg: &ArgInfo, index: usize) -> TokenStream {
    let arg_name = &arg.name;

    if arg.is_optional {
        if let Some(ref inner_ty) = arg.inner_ty {
            quote! {
                let #arg_name = args.parse_optional::<#inner_ty>(#index)
                    .map_err(::commands::CommandError::ParseError)?;
            }
        } else {
            let arg_ty = &arg.ty;
            quote! {
                let #arg_name: #arg_ty = args.parse_optional(#index)
                    .map_err(::commands::CommandError::ParseError)?;
            }
        }
    } else {
        let arg_ty = &arg.ty;
        quote! {
            let #arg_name = args.parse_arg::<#arg_ty>(#index)
                .map_err(::commands::CommandError::ParseError)?;
        }
    }
}

/// Generate completion arm for enum-based completion
fn generate_enum_completion_arm(
    arg_index: usize,
    arg: &ArgInfo,
    completers: &std::collections::HashMap<String, Ident>,
) -> Option<TokenStream> {
    let arg_name_str = arg.name.to_string();

    // Skip arguments with custom completers
    if completers.contains_key(&arg_name_str) {
        return None;
    }

    let completion_ty = get_effective_type(arg);

    // Skip non-completable built-in types
    if is_builtin_type(completion_ty) {
        return None;
    }

    Some(quote! {
        #arg_index => {
            // Use utility function to filter completions by partial match
            Ok(::commands::filter_completions(
                <#completion_ty as ::commands::Completable>::completions(),
                context.stream().partial()
            ))
        }
    })
}

/// Generate completion arm for custom completer
fn generate_custom_completion_arm(
    arg_index: usize,
    _arg: &ArgInfo,
    all_args: &[ArgInfo],
    completer_method: &Ident,
) -> TokenStream {
    // Parse all previous arguments using TokenStream.parse()
    let prev_args_parsing: Vec<_> = all_args.iter().take(arg_index).enumerate().map(|(j, prev_arg)| {
        let prev_name = &prev_arg.name;
        let prev_ty = get_effective_type(prev_arg);
        let token_index = j + 1; // +1 to skip command name

        quote! {
            let #prev_name = context.tokens.parse::<#prev_ty>(#token_index);
        }
    }).collect();

    let prev_arg_names: Vec<_> = all_args.iter().take(arg_index).map(|a| &a.name).collect();

    // Handle zero previous arguments case separately to avoid trailing comma issue
    if prev_arg_names.is_empty() {
        quote! {
            #arg_index => {
                target.#completer_method(partial).await
                    .map_err(|e| ::commands::CompletionError::Custom(e.to_string()))
            }
        }
    } else {
        let prev_arg_refs: Vec<_> = prev_arg_names.iter().map(|name| {
            quote! { &#name }
        }).collect();

        quote! {
            #arg_index => {
                #(#prev_args_parsing)*

                if vec![#(#prev_arg_names.is_some()),*].iter().all(|&x| x) {
                    target.#completer_method(#(#prev_arg_refs.unwrap()),*, partial).await
                        .map_err(|e| ::commands::CompletionError::Custom(e.to_string()))
                } else {
                    Ok(vec![])
                }
            }
        }
    }
}

/// Generate the command wrapper struct and implementations
fn generate_command_wrapper(info: &CommandInfo) -> TokenStream {
    let self_type = &info.self_type;
    let method_name = &info.method_name;
    let command_name = &info.command_name;
    let help_text = &info.help_text;

    // Generate wrapper struct name using DRY helper
    let wrapper_name = generate_wrapper_name(self_type, method_name);

    // Generate argument parsing code using DRY helper
    let parse_args = info.args.iter().enumerate()
        .map(|(i, arg)| generate_arg_parse(arg, i));

    // Generate argument names for method call
    let arg_names: Vec<_> = info.args.iter().map(|arg| &arg.name).collect();

    // Generate result conversion based on return type
    let result_conversion = generate_result_conversion(&info.return_type);

    // Generate completion routing for enum-based arguments using DRY helper
    let enum_completion_arms = info.args.iter().enumerate()
        .filter_map(|(i, arg)| generate_enum_completion_arm(i, arg, &info.completers));

    // Generate custom completer arms (if any)
    let completion_with_target_arms = info.args.iter().enumerate().filter_map(|(i, arg)| {
        let arg_name_str = arg.name.to_string();
        let completer_method = info.completers.get(&arg_name_str)?;
        Some(generate_custom_completion_arm(i, arg, &info.args, completer_method))
    });

    // Generate metadata method using DRY helper
    let metadata_impl = generate_metadata_impl(command_name, help_text);

    quote! {
        // Zero-sized command struct (no state!) - private, users don't need to know about it
        #[derive(Default)]
        #[doc(hidden)]
        pub(crate) struct #wrapper_name;

        #[::commands::async_trait::async_trait]
        impl ::commands::AsyncCompleter<#self_type> for #wrapper_name {
            async fn complete<'a>(
                &self,
                context: &::commands::context::Context<'a, #self_type>,
            ) -> ::std::result::Result<Vec<::commands::Completion>, ::commands::CompletionError> {
                let arg_index = context.stream().arg_index();
                let target = context.target;
                let partial = context.stream().partial();

                match arg_index {
                    #(#enum_completion_arms)*
                    #(#completion_with_target_arms)*
                    _ => Ok(vec![])
                }
            }
        }

        #[::commands::async_trait::async_trait]
        impl ::commands::Command<#self_type> for #wrapper_name {
            async fn execute<'a>(
                &self,
                mut context: ::commands::ContextMut<'a, #self_type>,
            ) -> ::std::result::Result<::commands::CommandResult, ::commands::CommandError> {
                let args = &context.tokens;

                // Parse arguments
                #(#parse_args)*

                // Call user method
                let target = &mut context.target;
                let result = target.#method_name(#(#arg_names),*).await;

                // Convert result to CommandResult
                #result_conversion
            }

            #metadata_impl
        }
    }
}

/// Generate result conversion code based on return type
fn generate_result_conversion(return_type: &ReturnType) -> TokenStream {
    match return_type {
        ReturnType::Default => {
            // No return type - infallible
            quote! {
                Ok(::commands::CommandResult::Success(String::new()))
            }
        }
        ReturnType::Type(_, ty) if is_result_type(ty) => {
            // Result<T, E> - need error handling
            let error_arm = quote! {
                Err(e) => Err(::commands::CommandError::ExecutionError(e.to_string()))
            };

            if is_result_unit_type(ty) {
                // Result<(), E>
                quote! {
                    match result {
                        Ok(()) => Ok(::commands::CommandResult::Success(String::new())),
                        #error_arm
                    }
                }
            } else {
                // Result<T, E> where T != ()
                quote! {
                    match result {
                        Ok(msg) => Ok(::commands::CommandResult::Success(format!("{}", msg))),
                        #error_arm
                    }
                }
            }
        }
        ReturnType::Type(_, _) => {
            // Direct return type - infallible
            quote! {
                Ok(::commands::CommandResult::Success(format!("{}", result)))
            }
        }
    }
}

/// Helper to check type properties - consolidates type introspection logic
fn check_type_path<F>(ty: &Type, checker: F) -> bool
where
    F: FnOnce(&syn::PathSegment) -> bool,
{
    matches!(ty, Type::Path(type_path) if type_path.path.segments.last().map(checker).unwrap_or(false))
}

/// Check if a type is a built-in type that doesn't support completion
fn is_builtin_type(ty: &Type) -> bool {
    check_type_path(ty, |segment| {
        matches!(
            segment.ident.to_string().as_str(),
            "String" | "str" | "i8" | "i16" | "i32" | "i64" | "i128" |
            "u8" | "u16" | "u32" | "u64" | "u128" | "usize" | "isize" |
            "f32" | "f64" | "bool" | "char"
        )
    })
}

/// Check if a type is Result<(), E>
fn is_result_unit_type(ty: &Type) -> bool {
    check_type_path(ty, |segment| {
        segment.ident == "Result" && matches!(
            &segment.arguments,
            syn::PathArguments::AngleBracketed(args) if matches!(
                args.args.first(),
                Some(syn::GenericArgument::Type(Type::Tuple(tuple))) if tuple.elems.is_empty()
            )
        )
    })
}

/// Check if a type is Result<T, E>
fn is_result_type(ty: &Type) -> bool {
    check_type_path(ty, |segment| segment.ident == "Result")
}

/// Convert snake_case to PascalCase
trait ToPascalCase {
    fn to_pascal_case(&self) -> String;
}

impl ToPascalCase for String {
    fn to_pascal_case(&self) -> String {
        self.split('_')
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    None => String::new(),
                    Some(first) => first.to_uppercase().chain(chars).collect(),
                }
            })
            .collect()
    }
}

impl ToPascalCase for str {
    fn to_pascal_case(&self) -> String {
        self.to_string().to_pascal_case()
    }
}

/// Generate wrapper struct name from self type and method name
fn generate_wrapper_name(self_type: &Type, method_name: &Ident) -> Ident {
    format_ident!(
        "{}{}Command",
        self_type.to_token_stream().to_string().replace(" ", ""),
        method_name.to_string().to_pascal_case()
    )
}

/// Generate metadata() method implementation
fn generate_metadata_impl(command_name: &str, help_text: &str) -> TokenStream {
    quote! {
        fn metadata(&self) -> &::commands::CommandMetadata {
            static METADATA: ::commands::CommandMetadata = ::commands::CommandMetadata {
                name: #command_name,
                help: #help_text,
            };
            &METADATA
        }
    }
}

/// Find a method with matching #[command(name = "...")] attribute
fn find_command_method<'a>(
    impl_block: &'a ItemImpl,
    command_name: &str,
) -> Result<&'a syn::ImplItemFn> {
    impl_block.items.iter()
        .find_map(|item| {
            if let ImplItem::Fn(method) = item {
                // Check if this method has matching command name
                for attr in &method.attrs {
                    if attr.path().is_ident("command") {
                        let mut matches = false;
                        let _ = attr.parse_nested_meta(|meta| {
                            if meta.path.is_ident("name") {
                                if let Ok(value) = meta.value() {
                                    if let Ok(s) = value.parse::<syn::LitStr>() {
                                        if s.value() == command_name {
                                            matches = true;
                                        }
                                    }
                                }
                            }
                            Ok(())
                        });
                        if matches {
                            return Some(method);
                        }
                    }
                }
            }
            None
        })
        .ok_or_else(|| {
            syn::Error::new(
                impl_block.span(),
                format!("No method with #[command(name = \"{}\")] found", command_name)
            )
        })
}

/// Extract command information for a specific command by name
fn extract_command_info_by_name(
    impl_block: &ItemImpl,
    command_name: &str,
    help_text: &str,
) -> Result<CommandInfo> {
    let self_type = if let Type::Path(type_path) = &*impl_block.self_ty {
        Type::Path(type_path.clone())
    } else {
        return Err(syn::Error::new(
            impl_block.self_ty.span(),
            "Expected a simple type path",
        ));
    };

    // Find the specific command method by name attribute using helper
    let method = find_command_method(impl_block, command_name)?;

    let method_name = method.sig.ident.clone();
    let return_type = method.sig.output.clone();

    // Parse arguments (skip self) using filter_map for cleaner code
    let args: Result<Vec<_>> = method.sig.inputs.iter()
        .filter_map(|arg| match arg {
            FnArg::Receiver(_) => None,
            FnArg::Typed(pat_type) => Some(parse_arg_info(pat_type)),
        })
        .collect();
    let args = args?;

    // Find completer methods for this command
    let completers = find_completers_for_command(impl_block, command_name)?;

    Ok(CommandInfo {
        method_name,
        self_type,
        args,
        return_type,
        command_name: command_name.to_string(),
        help_text: help_text.to_string(),
        completers,
    })
}

/// Find completer methods in the impl block for a specific command
/// Supports both:
/// - #[completer(command = "cmd", arg = "arg_name")] - command-specific completer
/// - #[completer(arg = "arg_name")] - global completer (backwards compatible, matches any command)
fn find_completers_for_command(
    impl_block: &ItemImpl,
    command_name: &str,
) -> Result<std::collections::HashMap<String, Ident>> {
    impl_block.items.iter()
        .filter_map(|item| {
            if let ImplItem::Fn(method) = item {
                // Parse #[completer(command = "...", arg = "arg_name")]
                let (completer_cmd, arg_name) = parse_completer_attribute(&method.attrs).ok()??;

                // Include this completer if:
                // 1. It has no command specified (global/backwards compatible), or
                // 2. Its command matches the one we're looking for
                let matches = completer_cmd.as_ref().is_none_or(|cmd| cmd == command_name);

                if matches {
                    Some(Ok((arg_name, method.sig.ident.clone())))
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect()
}

/// Strip #[command] and #[completer] attributes from impl block
fn strip_command_attrs(mut input: ItemImpl) -> ItemImpl {
    for item in &mut input.items {
        if let ImplItem::Fn(method) = item {
            method.attrs.retain(|attr| {
                !attr.path().is_ident("command") && !attr.path().is_ident("completer")
            });
        }
    }
    input
}

/// Implement the #[command] macro
pub fn impl_command(input: ItemImpl) -> Result<TokenStream> {
    // Parse command attributes
    let commands = parse_command_attrs(&input)?;

    if commands.is_empty() {
        // No commands found, return original impl block
        return Ok(quote! { #input });
    }

    // Get self type for helper method
    let self_type = &input.self_ty;

    // Strip #[command] attributes from the impl block for output
    let cleaned_input = strip_command_attrs(input.clone());

    // Generate wrappers for all commands
    let mut wrappers = Vec::new();
    let mut command_struct_names = Vec::new();

    for (command_name, help_text) in &commands {
        let info = extract_command_info_by_name(&input, command_name, help_text)?;
        let wrapper = generate_command_wrapper(&info);
        wrappers.push(wrapper);

        // Store struct name for registration helper using DRY helper
        let struct_name = generate_wrapper_name(self_type, &info.method_name);
        command_struct_names.push(struct_name);
    }

    // Generate Registrable trait impl
    let registrable_impl = quote! {
        impl ::commands::Registrable<::commands::CommandRegistry<#self_type>> for #self_type {
            fn register(registry: &mut ::commands::CommandRegistry<Self>) {
                #(registry.register(#command_struct_names::default());)*
            }
        }
    };

    Ok(quote! {
        #cleaned_input

        #(#wrappers)*

        #registrable_impl
    })
}

/// Implement the #[completer] macro
pub fn impl_completer(input: ImplItem) -> Result<TokenStream> {
    // This is a marker attribute for #[commandable] to detect
    // Just return the original method unchanged
    Ok(quote! {
        #input
    })
}
