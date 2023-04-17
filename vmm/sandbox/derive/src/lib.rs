/*
Copyright 2022 The Kuasar Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

#![allow(dead_code)]

#[macro_use]
extern crate quote;

use std::collections::HashMap;

use convert_case::{Case, Casing};
use proc_macro::TokenStream;
use proc_macro2::Ident;
use syn::{
    parse::Parse,
    parse_macro_input, Data, DataStruct, DeriveInput, Expr, ExprPath, Fields, Lit,
    Meta::{List, NameValue},
    NestedMeta,
};

#[allow(clippy::collapsible_else_if)]
#[proc_macro_derive(CmdLineParams, attributes(params, property))]
pub fn proc_macro_derive_params(item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as DeriveInput);
    let struct_name = &input.ident;
    let mut params = HashMap::new();
    input.attrs.iter()
        .filter(|&x| x.path.is_ident("params"))
        .for_each(|x| {
            match x.parse_meta() {
                Ok(List(meta)) => {
                    meta.nested.into_iter().for_each(|x| {
                        match x {
                            NestedMeta::Lit(l) => {
                                if let Lit::Str(s) = l {
                                    params.insert(s.value(), ParamAnnotation::new());
                                }
                            }
                            _ => {
                                panic!("compile error: the right form of params is #[params(\"parama\",\"paramb\")]");
                            }
                        }
                    })
                }
                Ok(_other) => {
                    panic!("compile error: the right form of params is #[params(\"parama\",\"paramb\")]");
                }
                Err(_) => {
                    panic!("compile error: the right form of params is #[params(\"parama\",\"paramb\")]");
                }
            }
        });

    if params.is_empty() {
        params.insert(
            struct_name.to_string().to_case(Case::Kebab),
            ParamAnnotation::new(),
        );
    }

    match input.data {
        Data::Struct(DataStruct { ref fields, .. }) => {
            if let Fields::Named(ref fields_named) = fields {
                fields_named.named.iter().for_each(|f| {
                    let mut ignore = false;
                    let mut no_property_attr = true;
                    f.attrs.iter().filter(|&attr| {
                        attr.path.is_ident("property")
                    }).for_each(|attr| {
                        no_property_attr = false;
                        match attr.parse_meta() {
                            Ok(List(meta)) => {
                                let mut param_anno = None;
                                let key = f.ident.clone().unwrap().to_string().trim_start_matches("r#").to_case(Case::Kebab);
                                let mut field = PropertyField {
                                    name: f.ident.clone().unwrap(),
                                    key,
                                    predicate: None,
                                    generator: None,
                                    ignore_key: false,
                                    is_option: false,
                                };
                                if is_option(&f.ty) {
                                    field.is_option = true;
                                }
                                let nested_metas = meta.nested.into_iter().collect::<Vec<NestedMeta>>();
                                for item in &nested_metas {
                                    match item {
                                        NestedMeta::Meta(NameValue(m)) if m.path.is_ident("param") => {
                                            if let Some(p) = get_lit_str(&m.lit) {
                                                param_anno = params.get_mut(&p.value());
                                                if param_anno.is_none() {
                                                    panic!("the param \"{}\" should be included in the params", p.value());
                                                }
                                            } else {
                                                panic!("param {m:?} is empty");
                                            }
                                        }
                                        NestedMeta::Meta(NameValue(m)) if m.path.is_ident("key") => {
                                            if let Some(l) = get_lit_str(&m.lit) {
                                                field.key = l.value();
                                            } else {
                                                panic!("key is empty")
                                            }
                                        }
                                        NestedMeta::Meta(NameValue(m)) if m.path.is_ident("predicate") => {
                                            if let Some(l) = get_lit_str(&m.lit) {
                                                field.predicate = Some(parse_litstr_into(l));
                                            } else {
                                                panic!("key is empty")
                                            }
                                        }
                                        NestedMeta::Meta(NameValue(m)) if m.path.is_ident("generator") => {
                                            if let Some(l) = get_lit_str(&m.lit) {
                                                field.generator = Some(parse_litstr_into(l));
                                            } else {
                                                panic!("key is empty")
                                            }
                                        }
                                        NestedMeta::Meta(syn::Meta::Path(p)) if p.is_ident("ignore_key") => {
                                            field.ignore_key = true;
                                        }
                                        NestedMeta::Meta(syn::Meta::Path(p)) if p.is_ident("ignore") => {
                                            ignore = true;
                                        }
                                        _ => {}
                                    }
                                };
                                if !ignore {
                                    match param_anno {
                                        None => {
                                            if params.len() == 1 {
                                                params.iter_mut().next().unwrap().1.fields.push(field);
                                            }
                                        }
                                        Some(anno) => {
                                            anno.fields.push(field);
                                        }
                                    }
                                }
                            }
                            _ => { panic!("expect #[property(key=\"xxx\",generator=\"predicate\")]") }
                        }
                    });
                    if !ignore && no_property_attr && params.len() == 1 {
                        let key = f.ident.clone().unwrap().to_string().trim_start_matches("r#").to_case(Case::Kebab);
                        let mut field = PropertyField {
                            name: f.ident.clone().unwrap(),
                            key,
                            predicate: None,
                            generator: None,
                            ignore_key: false,
                            is_option: false,
                        };
                        if is_option(&f.ty) {
                            field.is_option = true;
                        }
                        params.iter_mut().next().unwrap().1.fields.push(field);
                    }
                })
            } else {
                panic!("compile error: ToParams is only valid for structs with named fields");
            }
        }
        _ => {
            panic!("compile error: ToParams is only valid for struct");
        }
    };
    let add_param_codes: Vec<_> = params
        .iter()
        .map(|(x, ps)| {
            let set_fields_codes: Vec<_> = ps
                .fields
                .iter()
                .map(|f| {
                    let field_name = &f.name;
                    let value_codes = if let Some(g) = &f.generator {
                        if f.is_option {
                            // if the field is option and the value is none,
                            // we will not parse it to the param by default,
                            // so we unwrap() the value directly here
                            quote! {
                                prop.value = #g(self.#field_name.as_ref().unwrap());
                            }
                        } else {
                            quote! {
                                prop.value = #g(&self.#field_name);
                            }
                        }
                    } else {
                        if f.is_option {
                            // if the field is option and the value is none,
                            // we will not parse it to the param by default,
                            // so we unwrap() the value directly here
                            quote! {
                                prop.value = self.#field_name.as_ref().unwrap().to_string();
                            }
                        } else {
                            quote! {
                                prop.value = self.#field_name.to_string();
                            }
                        }
                    };
                    let ignore_key_codes = if f.ignore_key {
                        quote! {
                            prop.ignore_key = true;
                        }
                    } else {
                        quote! {}
                    };
                    let field_key = &f.key;
                    let new_param_codes = quote! {
                        let mut prop = crate::param::Property::new();
                        prop.key = #field_key.to_string();
                        #value_codes
                        #ignore_key_codes
                        param.properties.push(prop);
                    };
                    if let Some(p) = &f.predicate {
                        quote! {
                            if #p {
                                #new_param_codes
                            }
                        }
                    } else {
                        if f.is_option {
                            quote! {
                                if self.#field_name.is_some() {
                                    #new_param_codes
                                }
                            }
                        } else {
                            quote! {
                                {
                                    #new_param_codes
                                }
                            }
                        }
                    }
                })
                .collect();
            quote! {
                {
                    let mut param = crate::param::Param::new(#x);
                    #(#set_fields_codes)*
                    res.push(param);
                }
            }
        })
        .collect();
    let expand = quote! {
        impl crate::param::ToParams for #struct_name {
            fn to_params(&self) -> Vec<crate::param::Param> {
                let mut res = vec![];
                #(#add_param_codes)*
                return res;
            }
        }
    };
    expand.into()
}

#[proc_macro_derive(CmdLineParamSet, attributes(param))]
pub fn proc_macro_derive_param_set(item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as DeriveInput);
    let struct_name = &input.ident;
    let mut params = vec![];
    match input.data {
        Data::Struct(DataStruct { ref fields, .. }) => {
            if let Fields::Named(ref fields_named) = fields {
                fields_named.named.iter().for_each(|f| {
                    let mut ignore = false;
                    let mut key = f
                        .ident
                        .clone()
                        .unwrap()
                        .to_string()
                        .trim_start_matches("r#")
                        .to_case(Case::Kebab);
                    let is_bool = is_bool(&f.ty);
                    let is_primitive = is_primitives(&f.ty);
                    let is_vec = is_vec(&f.ty);
                    let is_option = is_option(&f.ty);
                    f.attrs
                        .iter()
                        .filter(|&attr| attr.path.is_ident("param"))
                        .for_each(|attr| {
                            if let Ok(List(meta)) = attr.parse_meta() {
                                let nested_metas =
                                    meta.nested.into_iter().collect::<Vec<NestedMeta>>();
                                for item in &nested_metas {
                                    match item {
                                        NestedMeta::Meta(NameValue(m))
                                            if m.path.is_ident("key") =>
                                        {
                                            if let Some(l) = get_lit_str(&m.lit) {
                                                key = l.value();
                                            }
                                        }
                                        NestedMeta::Meta(syn::Meta::Path(p))
                                            if p.is_ident("ignore") =>
                                        {
                                            ignore = true;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        });
                    if !ignore {
                        let field = if is_bool {
                            ParamField::Bool {
                                key,
                                name: f.ident.clone().unwrap(),
                            }
                        } else if is_primitive {
                            ParamField::String {
                                key,
                                name: f.ident.clone().unwrap(),
                            }
                        } else if is_vec {
                            ParamField::Vec {
                                key,
                                name: f.ident.clone().unwrap(),
                            }
                        } else if is_option {
                            ParamField::Option {
                                key,
                                name: f.ident.clone().unwrap(),
                            }
                        } else {
                            ParamField::Trait {
                                name: f.ident.clone().unwrap(),
                            }
                        };
                        params.push(field);
                    }
                });
            } else {
                panic!("compile error: ToParams is only valid for structs with named fields");
            }
        }
        _ => {
            panic!("compile error: ToParams is only valid for struct");
        }
    };

    let add_param_codes: Vec<_> = params
        .iter()
        .map(|f| {
            match f {
                ParamField::String { key, name } => {
                    quote! {
                        {
                            let mut param = crate::param::StringParam::new(#key, self.#name.to_string());
                            res.append(&mut param.to_cmdline_params(hyphen));
                        }
                    }
                }
                ParamField::Trait { name } => {
                    quote! {
                            res.append(&mut self.#name.to_cmdline_params(hyphen));
                    }
                }
                ParamField::Bool { key, name } => {
                    quote! {
                        {
                            let mut param = crate::param::BoolParam::new(#key, self.#name);
                            res.append(&mut param.to_cmdline_params(hyphen));
                        }
                    }
                }
                ParamField::Vec { key, name } => {
                    quote! {
                        {
                            let mut param = crate::param::VecParam::new(#key, self.#name.clone());
                            res.append(&mut param.to_cmdline_params(hyphen));
                        }
                    }
                }
                ParamField::Option { key, name } => {
                    quote! {
                        {
                            let mut param = crate::param::OptionParam::new(#key, self.#name.clone());
                            res.append(&mut param.to_cmdline_params(hyphen));
                        }
                    }
                }
            }
        }).collect();
    let expand = quote! {
        impl crate::param::ToCmdLineParams for #struct_name {
            fn to_cmdline_params(&self, hyphen: &str) -> Vec<String> {
                let mut res = vec![];
                #(#add_param_codes)*
                return res;
            }
        }
    };
    expand.into()
}

struct ParamAnnotation {
    fields: Vec<PropertyField>,
}

impl ParamAnnotation {
    fn new() -> ParamAnnotation {
        ParamAnnotation { fields: vec![] }
    }
}

struct PropertyField {
    name: Ident,
    key: String,
    predicate: Option<Expr>,
    generator: Option<ExprPath>,
    ignore_key: bool,
    is_option: bool,
}

enum ParamField {
    String { key: String, name: Ident },
    Trait { name: Ident },
    Bool { key: String, name: Ident },
    Vec { key: String, name: Ident },
    Option { key: String, name: Ident },
}

fn get_lit_str(lit: &syn::Lit) -> Option<&syn::LitStr> {
    if let Lit::Str(lit) = lit {
        return Some(lit);
    }
    None
}

fn parse_litstr_into<T: Parse>(l: &syn::LitStr) -> T {
    let res = syn::parse_str(&l.value()).and_then(|t| syn::parse2(t));
    match res {
        Ok(ex) => ex,
        Err(e) => panic!("failed to parse the predicate {l:?}, err: {e}"),
    }
}

fn is_option(ty: &syn::Type) -> bool {
    let path = match ungroup(ty) {
        syn::Type::Path(ty) => &ty.path,
        _ => {
            return false;
        }
    };
    let seg = match path.segments.last() {
        Some(seg) => seg,
        None => {
            return false;
        }
    };
    seg.ident == "Option"
}

fn is_bool(ty: &syn::Type) -> bool {
    let path = match ungroup(ty) {
        syn::Type::Path(ty) => &ty.path,
        _ => {
            return false;
        }
    };
    if path.segments.len() != 1 {
        return false;
    }
    let seg = match path.segments.last() {
        Some(seg) => seg,
        None => {
            return false;
        }
    };
    seg.ident == "bool"
}

const PRIMITIVE_TYPES: [&str; 18] = [
    "String", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128",
    "isize", "f8", "f16", "f32", "f64", "f128",
];

fn is_primitives(ty: &syn::Type) -> bool {
    let path = match ungroup(ty) {
        syn::Type::Path(ty) => &ty.path,
        _ => {
            return false;
        }
    };
    if path.segments.len() != 1 {
        return false;
    }
    let seg = match path.segments.last() {
        Some(seg) => seg,
        None => {
            return false;
        }
    };
    PRIMITIVE_TYPES.contains(&&*seg.ident.to_string())
}

fn is_vec(ty: &syn::Type) -> bool {
    let path = match ungroup(ty) {
        syn::Type::Path(ty) => &ty.path,
        _ => {
            return false;
        }
    };
    let seg = match path.segments.last() {
        Some(seg) => seg,
        None => {
            return false;
        }
    };
    seg.ident == "Vec"
}

fn ungroup(mut ty: &syn::Type) -> &syn::Type {
    while let syn::Type::Group(group) = ty {
        ty = &group.elem;
    }
    ty
}
