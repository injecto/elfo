use std::time::UNIX_EPOCH;

use proc_macro::TokenStream;
use quote::quote;
use syn::{
    parenthesized,
    parse::{Error as ParseError, Parse, ParseStream},
    parse_macro_input, parse_quote,
    punctuated::Punctuated,
    DeriveInput, Ident, Path, Token, Type,
};

#[derive(Debug)]
struct MessageArgs {
    ret: Option<Type>,
    crate_: Path,
}

impl Parse for MessageArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self, ParseError> {
        // TODO: support any order of attributes.

        let mut args = MessageArgs {
            ret: None,
            crate_: parse_quote!(::elfo),
        };

        // `#[message]`
        // `#[message(ret(A))]`
        // `#[message(ret(A), crate = "some")]`
        // `#[message(crate = some::path)]`
        while !input.is_empty() {
            let ident: Ident = input.parse()?;

            match ident.to_string().as_str() {
                "ret" => {
                    let _: Token![=] = input.parse()?;
                    args.ret = Some(input.parse()?);
                }
                // TODO: call it `crate` like in linkme?
                "elfo" => {
                    let _: Token![=] = input.parse()?;
                    args.crate_ = input.parse()?;
                }
                attr => panic!("invalid attribute: {}", attr),
            }

            if !input.is_empty() {
                let _: Token![,] = input.parse()?;
            }
        }

        Ok(args)
    }
}

fn gen_ltid() -> u32 {
    // TODO
    let elapsed = UNIX_EPOCH.elapsed().expect("invalid system time");
    elapsed.as_nanos() as u32
}

pub fn message_impl(args: TokenStream, input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args as MessageArgs);

    // TODO: what about parsing into something cheaper?
    let input = parse_macro_input!(input as DeriveInput);
    let name = input.ident.clone();
    let mod_name = Ident::new(&format!("_elfo_{}", name), name.span());
    let ltid = gen_ltid();
    let crate_ = args.crate_;

    let derive_request = if let Some(ret) = args.ret {
        quote! {
            impl #crate_::Request for #name {
                type Response = #ret;
            }
        }
    } else {
        quote! {}
    };

    // TODO: impl `Serialize` and `Deserialize`.
    TokenStream::from(quote! {
        #[derive(Clone)]
        #input

        impl #crate_::Message for #name {
            const _LTID: #crate_::_priv::LocalTypeId = #ltid;
        }

        #[allow(non_snake_case)]
        mod #mod_name {
            use super::#name;

            use #crate_::_priv::{MESSAGE_LIST, MessageVTable, smallbox::{smallbox}, AnyMessage, linkme};

            fn clone(message: &AnyMessage) -> AnyMessage {
                smallbox!(message.downcast_ref::<#name>().expect("invalid vtable").clone())
            }

            #[linkme::distributed_slice(MESSAGE_LIST)]
            #[linkme(crate = #crate_::_priv::linkme)]
            static VTABLE: MessageVTable = MessageVTable {
                ltid: #ltid,
                clone,
            };
        }

        #derive_request
    })
}
