use crate::{
    attributes::{parse_attributes, Attributes},
    repr::{IntRepr, Repr, ReprAttr},
    with::{make_with_cast, make_with_ty},
};
use proc_macro2::TokenStream;
use quote::{quote, quote_spanned};
use syn::{
    parse_quote, spanned::Spanned, Attribute, Data, DeriveInput, Error, Field, Fields, Ident,
    Index, Meta, NestedMeta, Type,
};

pub fn derive(input: DeriveInput) -> Result<TokenStream, Error> {
    let attributes = parse_attributes(&input)?;
    derive_archive_impl(input, &attributes)
}

fn field_archive_attrs(field: &Field) -> impl '_ + Iterator<Item = NestedMeta> {
    field
        .attrs
        .iter()
        .filter_map(|attr| {
            if let Ok(Meta::List(list)) = attr.parse_meta() {
                if list.path.is_ident("archive_attr") {
                    Some(list.nested)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .flatten()
}

fn derive_archive_impl(
    mut input: DeriveInput,
    attributes: &Attributes,
) -> Result<TokenStream, Error> {
    input.generics.make_where_clause();

    let name = &input.ident;
    let vis = &input.vis;
    let generics = &input.generics;

    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let where_clause = where_clause.unwrap();

    let default_rkyv_path = parse_quote! { ::rkyv };
    let rkyv_path = attributes.rkyv_path.as_ref().unwrap_or(&default_rkyv_path);
    let with_ty = make_with_ty(rkyv_path);
    let with_cast = make_with_cast(rkyv_path);

    let archive_attrs = attributes
        .attrs
        .iter()
        .map::<Attribute, _>(|d| parse_quote! { #[#d] });

    if let Some(ref archive_as) = attributes.archive_as {
        if let Some(ref ident) = attributes.archived {
            return Err(Error::new_spanned(
                ident,
                "archived = \"...\" may not be used with as = \"...\" because no type is generated",
            ));
        }
        if !attributes.attrs.is_empty() {
            return Err(Error::new_spanned(
                name,
                format!("archive_attr(...) may not be used with as = \"...\"\nplace any attributes on the archived type ({}) instead", archive_as.value()),
            ));
        }
        if attributes.archived_repr.is_some() {
            return Err(Error::new_spanned(
                name,
                format!("repr(...) may not be used with as = \"...\"\nplace the repr attribute on the archived type ({}) instead", archive_as.value()),
            ));
        }
    }

    let archived_name = attributes.archived.as_ref().map_or_else(
        || Ident::new(&format!("Archived{}", name), name.span()),
        |value| value.clone(),
    );
    let archived_doc = format!("An archived `{}`", name);

    let archived_type = attributes.archive_as.as_ref().map_or_else(
        || Ok(parse_quote! { #archived_name #ty_generics }),
        |lit| lit.parse::<Type>(),
    )?;

    let resolver = attributes.resolver.as_ref().map_or_else(
        || Ident::new(&format!("{}Resolver", name), name.span()),
        |value| value.clone(),
    );
    let resolver_doc = format!("The resolver for archived `{}`", name);

    let (archive_types, archive_impls) = match input.data {
        Data::Struct(ref data) => {
            let is_strict = cfg!(feature = "strict");
            let is_strict_repr = matches!(
                attributes.archived_repr,
                None | Some(ReprAttr {
                    repr: Repr::C,
                    span: _
                }) | Some(ReprAttr {
                    repr: Repr::Transparent,
                    span: _
                })
            );
            if is_strict && !is_strict_repr {
                return Err(Error::new_spanned(
                    name,
                    "archived structs may only be repr(C) in strict mode",
                ));
            }

            let repr = if is_strict {
                Some(Repr::C)
            } else {
                attributes
                    .archived_repr
                    .as_ref()
                    .map(|repr_attr| repr_attr.repr)
            };

            match data.fields {
                Fields::Named(ref fields) => {
                    let mut archive_where = where_clause.clone();
                    for field in fields
                        .named
                        .iter()
                        .filter(|f| !f.attrs.iter().any(|a| a.path.is_ident("omit_bounds")))
                    {
                        let ty = with_ty(field);
                        archive_where
                            .predicates
                            .push(parse_quote! { #ty: #rkyv_path::Archive });
                    }

                    let resolver_fields = fields.named.iter().map(|f| {
                        let name = &f.ident;
                        let ty = with_ty(f);
                        quote_spanned! { f.span() => #name: #rkyv_path::Resolver<#ty> }
                    });

                    let archived_def = if attributes.archive_as.is_none() {
                        let archived_fields = fields.named.iter().map(|f| {
                            let field_name = f.ident.as_ref();
                            let ty = with_ty(f);
                            let vis = &f.vis;
                            let field_doc = format!(
                                "The archived counterpart of `{}::{}`",
                                name,
                                field_name.unwrap()
                            );
                            let archive_attrs = field_archive_attrs(f);
                            quote_spanned! { f.span() =>
                                #[doc = #field_doc]
                                #(#[#archive_attrs])*
                                #vis #field_name: #rkyv_path::Archived<#ty>
                            }
                        });

                        Some(quote! {
                            #[automatically_derived]
                            #[doc = #archived_doc]
                            #(#archive_attrs)*
                            #repr
                            #vis struct #archived_name #generics #archive_where {
                                #(#archived_fields,)*
                            }
                        })
                    } else {
                        None
                    };

                    let resolve_fields = fields.named.iter().map(|f| {
                        let name = &f.ident;
                        let field = with_cast(f, parse_quote! { &self.#name });
                        quote_spanned! { f.span() =>
                            let (fp, fo) = out_field!(out.#name);
                            #field.resolve(pos + fp, resolver.#name, fo);
                        }
                    });

                    let mut partial_eq_impl = None;
                    let mut partial_ord_impl = None;
                    if let Some((_, ref compares)) = attributes.compares {
                        for compare in compares {
                            if compare.is_ident("PartialEq") {
                                let mut partial_eq_where = archive_where.clone();
                                for field in fields.named.iter().filter(|f| {
                                    !f.attrs.iter().any(|a| a.path.is_ident("omit_bounds"))
                                }) {
                                    let ty = &field.ty;
                                    let wrapped_ty = with_ty(field);
                                    partial_eq_where.predicates.push(
                                        parse_quote! { Archived<#wrapped_ty>: PartialEq<#ty> },
                                    );
                                }

                                let field_names = fields.named.iter().map(|f| &f.ident);

                                partial_eq_impl = Some(quote! {
                                    impl #impl_generics PartialEq<#archived_type> for #name #ty_generics #partial_eq_where {
                                        #[inline]
                                        fn eq(&self, other: &#archived_type) -> bool {
                                            true #(&& other.#field_names.eq(&self.#field_names))*
                                        }
                                    }

                                    impl #impl_generics PartialEq<#name #ty_generics> for #archived_type #partial_eq_where {
                                        #[inline]
                                        fn eq(&self, other: &#name #ty_generics) -> bool {
                                            other.eq(self)
                                        }
                                    }
                                });
                            } else if compare.is_ident("PartialOrd") {
                                let mut partial_ord_where = archive_where.clone();
                                for field in fields.named.iter().filter(|f| {
                                    !f.attrs.iter().any(|a| a.path.is_ident("omit_bounds"))
                                }) {
                                    let ty = &field.ty;
                                    let archived_ty = with_ty(field);
                                    partial_ord_where.predicates.push(
                                        parse_quote! { Archived<#archived_ty>: PartialOrd<#ty> },
                                    );
                                }

                                let field_names = fields.named.iter().map(|f| &f.ident);

                                partial_ord_impl = Some(quote! {
                                    impl #impl_generics PartialOrd<#archived_type> for #name #ty_generics #partial_ord_where {
                                        #[inline]
                                        fn partial_cmp(&self, other: &#archived_type) -> Option<::core::cmp::Ordering> {
                                            #(
                                                match other.#field_names.partial_cmp(&self.#field_names) {
                                                    Some(::core::cmp::Ordering::Equal) => (),
                                                    x => return x,
                                                }
                                            )*
                                            Some(::core::cmp::Ordering::Equal)
                                        }
                                    }

                                    impl #impl_generics PartialOrd<#name #ty_generics> for #archived_type #partial_ord_where {
                                        #[inline]
                                        fn partial_cmp(&self, other: &#name #ty_generics) -> Option<::core::cmp::Ordering> {
                                            other.partial_cmp(self)
                                        }
                                    }
                                });
                            } else {
                                return Err(Error::new_spanned(compare, "unrecognized compare argument, supported compares are PartialEq and PartialOrd"));
                            }
                        }
                    }

                    let copy_safe_impl = if cfg!(feature = "copy") && attributes.copy_safe.is_some()
                    {
                        let mut copy_safe_where = where_clause.clone();
                        for field in fields
                            .named
                            .iter()
                            .filter(|f| !f.attrs.iter().any(|a| a.path.is_ident("omit_bounds")))
                        {
                            let ty = with_ty(field);
                            copy_safe_where
                                .predicates
                                .push(parse_quote! { #ty: #rkyv_path::copy::ArchiveCopySafe });
                        }

                        Some(quote! {
                            unsafe impl #impl_generics #rkyv_path::copy::ArchiveCopySafe for #name #ty_generics #copy_safe_where {}
                        })
                    } else {
                        None
                    };

                    (
                        quote! {
                            #archived_def

                            #[automatically_derived]
                            #[doc = #resolver_doc]
                            #vis struct #resolver #generics #archive_where {
                                #(#resolver_fields,)*
                            }
                        },
                        quote! {
                            impl #impl_generics Archive for #name #ty_generics #archive_where {
                                type Archived = #archived_type;
                                type Resolver = #resolver #ty_generics;

                                // Some resolvers will be (), this allow is to prevent clippy from complaining
                                #[allow(clippy::unit_arg)]
                                #[inline]
                                unsafe fn resolve(&self, pos: usize, resolver: Self::Resolver, out: &mut MaybeUninit<Self::Archived>) {
                                    #(#resolve_fields)*
                                }
                            }

                            #partial_eq_impl
                            #partial_ord_impl
                            #copy_safe_impl
                        },
                    )
                }
                Fields::Unnamed(ref fields) => {
                    let mut archive_where = where_clause.clone();
                    for field in fields
                        .unnamed
                        .iter()
                        .filter(|f| !f.attrs.iter().any(|a| a.path.is_ident("omit_bounds")))
                    {
                        let ty = with_ty(field);
                        archive_where
                            .predicates
                            .push(parse_quote! { #ty: #rkyv_path::Archive });
                    }

                    let resolver_fields = fields.unnamed.iter().map(|f| {
                        let ty = with_ty(f);
                        quote_spanned! { f.span() => #rkyv_path::Resolver<#ty> }
                    });

                    let archived_def = if attributes.archive_as.is_none() {
                        let archived_fields = fields.unnamed.iter().enumerate().map(|(i, f)| {
                            let ty = with_ty(f);
                            let vis = &f.vis;
                            let field_doc =
                                format!("The archived counterpart of `{}::{}`", name, i);
                            let archive_attrs = field_archive_attrs(f);
                            quote_spanned! { f.span() =>
                                #[doc = #field_doc]
                                #(#[#archive_attrs])*
                                #vis #rkyv_path::Archived<#ty>
                            }
                        });

                        Some(quote! {
                            #[automatically_derived]
                            #[doc = #archived_doc]
                            #(#archive_attrs)*
                            #repr
                            #vis struct #archived_name #generics (#(#archived_fields,)*) #archive_where;
                        })
                    } else {
                        None
                    };

                    let resolve_fields = fields.unnamed.iter().enumerate().map(|(i, f)| {
                        let index = Index::from(i);
                        let field = with_cast(f, parse_quote! { &self.#index });
                        quote_spanned! { f.span() =>
                            let (fp, fo) = out_field!(out.#index);
                            #field.resolve(pos + fp, resolver.#index, fo);
                        }
                    });

                    let mut partial_eq_impl = None;
                    let mut partial_ord_impl = None;
                    if let Some((_, ref compares)) = attributes.compares {
                        for compare in compares {
                            if compare.is_ident("PartialEq") {
                                let mut partial_eq_where = archive_where.clone();
                                for field in fields.unnamed.iter().filter(|f| {
                                    !f.attrs.iter().any(|a| a.path.is_ident("omit_bounds"))
                                }) {
                                    let ty = with_ty(field);
                                    partial_eq_where
                                        .predicates
                                        .push(parse_quote! { Archived<#ty>: PartialEq<#ty> });
                                }

                                let field_names = fields
                                    .unnamed
                                    .iter()
                                    .enumerate()
                                    .map(|(i, _)| Index::from(i));

                                partial_eq_impl = Some(quote! {
                                    impl #impl_generics PartialEq<#archived_type> for #name #ty_generics #partial_eq_where {
                                        #[inline]
                                        fn eq(&self, other: &#archived_type) -> bool {
                                            true #(&& other.#field_names.eq(&self.#field_names))*
                                        }
                                    }

                                    impl #impl_generics PartialEq<#name #ty_generics> for #archived_type #partial_eq_where {
                                        #[inline]
                                        fn eq(&self, other: &#name #ty_generics) -> bool {
                                            other.eq(self)
                                        }
                                    }
                                });
                            } else if compare.is_ident("PartialOrd") {
                                let mut partial_ord_where = archive_where.clone();
                                for field in fields.unnamed.iter().filter(|f| {
                                    !f.attrs.iter().any(|a| a.path.is_ident("omit_bounds"))
                                }) {
                                    let ty = with_ty(field);
                                    partial_ord_where
                                        .predicates
                                        .push(parse_quote! { Archived<#ty>: PartialOrd<#ty> });
                                }

                                let field_names = fields
                                    .unnamed
                                    .iter()
                                    .enumerate()
                                    .map(|(i, _)| Index::from(i));

                                partial_ord_impl = Some(quote! {
                                    impl #impl_generics PartialOrd<#archived_type> for #name #ty_generics #partial_ord_where {
                                        #[inline]
                                        fn partial_cmp(&self, other: &#archived_type) -> Option<::core::cmp::Ordering> {
                                            #(
                                                match other.#field_names.partial_cmp(&self.#field_names) {
                                                    Some(::core::cmp::Ordering::Equal) => (),
                                                    x => return x,
                                                }
                                            )*
                                            Some(::core::cmp::Ordering::Equal)
                                        }
                                    }

                                    impl #impl_generics PartialOrd<#name #ty_generics> for #archived_type #partial_ord_where {
                                        #[inline]
                                        fn partial_cmp(&self, other: &#name #ty_generics) -> Option<::core::cmp::Ordering> {
                                            other.partial_cmp(self)
                                        }
                                    }
                                });
                            } else {
                                return Err(Error::new_spanned(compare, "unrecognized compare argument, supported compares are PartialEq and PartialOrd"));
                            }
                        }
                    }

                    let copy_safe_impl = if cfg!(feature = "copy") && attributes.copy_safe.is_some()
                    {
                        let mut copy_safe_where = where_clause.clone();
                        for field in fields
                            .unnamed
                            .iter()
                            .filter(|f| !f.attrs.iter().any(|a| a.path.is_ident("omit_bounds")))
                        {
                            let ty = with_ty(field);
                            copy_safe_where
                                .predicates
                                .push(parse_quote! { #ty: #rkyv_path::copy::ArchiveCopySafe });
                        }

                        Some(quote! {
                            unsafe impl #impl_generics #rkyv_path::copy::ArchiveCopySafe for #name #ty_generics #copy_safe_where {}
                        })
                    } else {
                        None
                    };

                    (
                        quote! {
                            #archived_def

                            #[automatically_derived]
                            #[doc = #resolver_doc]
                            #vis struct #resolver #generics (#(#resolver_fields,)*) #archive_where;
                        },
                        quote! {
                            impl #impl_generics Archive for #name #ty_generics #archive_where {
                                type Archived = #archived_type;
                                type Resolver = #resolver #ty_generics;

                                // Some resolvers will be (), this allow is to prevent clippy from complaining
                                #[allow(clippy::unit_arg)]
                                #[inline]
                                unsafe fn resolve(&self, pos: usize, resolver: Self::Resolver, out: &mut MaybeUninit<Self::Archived>) {
                                    #(#resolve_fields)*
                                }
                            }

                            #partial_eq_impl
                            #partial_ord_impl
                            #copy_safe_impl
                        },
                    )
                }
                Fields::Unit => {
                    let archived_def = if attributes.archive_as.is_none() {
                        Some(quote! {
                            #[automatically_derived]
                            #[doc = #archived_doc]
                            #(#archive_attrs)*
                            #repr
                            #vis struct #archived_name #generics
                            #where_clause;
                        })
                    } else {
                        None
                    };

                    let mut partial_eq_impl = None;
                    let mut partial_ord_impl = None;
                    if let Some((_, ref compares)) = attributes.compares {
                        for compare in compares {
                            if compare.is_ident("PartialEq") {
                                partial_eq_impl = Some(quote! {
                                    impl #impl_generics PartialEq<#archived_type> for #name #ty_generics #where_clause {
                                        #[inline]
                                        fn eq(&self, _: &#archived_type) -> bool {
                                            true
                                        }
                                    }

                                    impl #impl_generics PartialEq<#name #ty_generics> for #archived_type #where_clause {
                                        #[inline]
                                        fn eq(&self, _: &#name #ty_generics) -> bool {
                                            true
                                        }
                                    }
                                });
                            } else if compare.is_ident("PartialOrd") {
                                partial_ord_impl = Some(quote! {
                                    impl #impl_generics PartialOrd<#archived_type> for #name #ty_generics #where_clause {
                                        #[inline]
                                        fn partial_cmp(&self, _: &#archived_type) -> Option<::core::cmp::Ordering> {
                                            Some(::core::cmp::Ordering::Equal)
                                        }
                                    }

                                    impl #impl_generics PartialOrd<#name #ty_generics> for #archived_type #where_clause {
                                        #[inline]
                                        fn partial_cmp(&self, _:&#name #ty_generics) -> Option<::core::cmp::Ordering> {
                                            Some(::core::cmp::Ordering::Equal)
                                        }
                                    }
                                });
                            } else {
                                return Err(Error::new_spanned(compare, "unrecognized compare argument, supported compares are PartialEq and PartialOrd"));
                            }
                        }
                    }

                    let copy_safe_impl = if cfg!(feature = "copy") && attributes.copy_safe.is_some()
                    {
                        Some(quote! {
                            unsafe impl #impl_generics #rkyv_path::copy::ArchiveCopySafe for #name #ty_generics #where_clause {}
                        })
                    } else {
                        None
                    };

                    (
                        quote! {
                            #archived_def

                            #[automatically_derived]
                            #[doc = #resolver_doc]
                            #vis struct #resolver #generics
                            #where_clause;
                        },
                        quote! {
                            impl #impl_generics Archive for #name #ty_generics #where_clause {
                                type Archived = #archived_type;
                                type Resolver = #resolver #ty_generics;

                                #[inline]
                                unsafe fn resolve(&self, _: usize, _: Self::Resolver, _: &mut MaybeUninit<Self::Archived>) {}
                            }

                            #partial_eq_impl
                            #partial_ord_impl
                            #copy_safe_impl
                        },
                    )
                }
            }
        }
        Data::Enum(ref data) => {
            let mut archive_where = where_clause.clone();
            for variant in data.variants.iter() {
                match variant.fields {
                    Fields::Named(ref fields) => {
                        for field in fields
                            .named
                            .iter()
                            .filter(|f| !f.attrs.iter().any(|a| a.path.is_ident("omit_bounds")))
                        {
                            let ty = with_ty(field);
                            archive_where
                                .predicates
                                .push(parse_quote! { #ty: #rkyv_path::Archive });
                        }
                    }
                    Fields::Unnamed(ref fields) => {
                        for field in fields
                            .unnamed
                            .iter()
                            .filter(|f| !f.attrs.iter().any(|a| a.path.is_ident("omit_bounds")))
                        {
                            let ty = with_ty(field);
                            archive_where
                                .predicates
                                .push(parse_quote! { #ty: #rkyv_path::Archive });
                        }
                    }
                    Fields::Unit => (),
                }
            }

            let resolver_variants = data.variants.iter().map(|v| {
                let variant = &v.ident;
                match v.fields {
                    Fields::Named(ref fields) => {
                        let fields = fields.named.iter().map(|f| {
                            let name = &f.ident;
                            let ty = with_ty(f);
                            quote_spanned! { f.span() => #name: #rkyv_path::Resolver<#ty> }
                        });
                        quote_spanned! { variant.span() =>
                            #[allow(dead_code)]
                            #variant {
                                #(#fields,)*
                            }
                        }
                    }
                    Fields::Unnamed(ref fields) => {
                        let fields = fields.unnamed.iter().map(|f| {
                            let ty = with_ty(f);
                            quote_spanned! { f.span() => #rkyv_path::Resolver<#ty> }
                        });
                        quote_spanned! { variant.span() =>
                            #[allow(dead_code)]
                            #variant(#(#fields,)*)
                        }
                    }
                    Fields::Unit => quote_spanned! { variant.span() =>
                        #[allow(dead_code)]
                        #variant
                    },
                }
            });

            let resolve_arms = data.variants.iter().map(|v| {
                let variant = &v.ident;
                let archived_variant_name = Ident::new(&format!("ArchivedVariant{}", variant.to_string()), v.span());
                match v.fields {
                    Fields::Named(ref fields) => {
                        let self_bindings = fields.named.iter().map(|f| {
                            let name = &f.ident;
                            let binding = Ident::new(&format!("self_{}", name.as_ref().unwrap().to_string()), name.span());
                            quote_spanned! { name.span() => #name: #binding }
                        });
                        let resolver_bindings = fields.named.iter().map(|f| {
                            let name = &f.ident;
                            let binding = Ident::new(&format!("resolver_{}", name.as_ref().unwrap().to_string()), name.span());
                            quote_spanned! { binding.span() => #name: #binding }
                        });
                        let resolves = fields.named.iter().map(|f| {
                            let name = &f.ident;
                            let self_binding = Ident::new(&format!("self_{}", name.as_ref().unwrap().to_string()), name.span());
                            let resolver_binding = Ident::new(&format!("resolver_{}", name.as_ref().unwrap().to_string()), name.span());
                            let value = with_cast(f, parse_quote! { #self_binding });
                            quote! {
                                let (fp, fo) = out_field!(out.#name);
                                #value.resolve(pos + fp, #resolver_binding, fo);
                            }
                        });
                        quote_spanned! { name.span() =>
                            #resolver::#variant { #(#resolver_bindings,)* } => {
                                match self {
                                    #name::#variant { #(#self_bindings,)* } => {
                                        let out = &mut *out.as_mut_ptr().cast::<MaybeUninit<#archived_variant_name #ty_generics>>();
                                        ::core::ptr::addr_of_mut!((*out.as_mut_ptr()).__tag)
                                            .write(ArchivedTag::#variant);
                                        #(#resolves)*
                                    },
                                    #[allow(unreachable_patterns)]
                                    _ => ::core::hint::unreachable_unchecked(),
                                }
                            }
                        }
                    }
                    Fields::Unnamed(ref fields) => {
                        let self_bindings = fields.unnamed.iter().enumerate().map(|(i, f)| {
                            let name = Ident::new(&format!("self_{}", i), f.span());
                            quote_spanned! { f.span() => #name }
                        });
                        let resolver_bindings = fields.unnamed.iter().enumerate().map(|(i, f)| {
                            let name = Ident::new(&format!("resolver_{}", i), f.span());
                            quote_spanned! { f.span() => #name }
                        });
                        let resolves = fields.unnamed.iter().enumerate().map(|(i, f)| {
                            let index = Index::from(i + 1);
                            let self_binding = Ident::new(&format!("self_{}", i), f.span());
                            let resolver_binding = Ident::new(&format!("resolver_{}", i), f.span());
                            let value = with_cast(f, parse_quote! { #self_binding });
                            quote! {
                                let (fp, fo) = out_field!(out.#index);
                                #value.resolve(pos + fp, #resolver_binding, fo);
                            }
                        });
                        quote_spanned! { name.span() =>
                            #resolver::#variant( #(#resolver_bindings,)* ) => {
                                match self {
                                    #name::#variant(#(#self_bindings,)*) => {
                                        let out = &mut *out.as_mut_ptr().cast::<MaybeUninit<#archived_variant_name #ty_generics>>();
                                        ::core::ptr::addr_of_mut!((*out.as_mut_ptr()).0).write(ArchivedTag::#variant);
                                        #(#resolves)*
                                    },
                                    #[allow(unreachable_patterns)]
                                    _ => ::core::hint::unreachable_unchecked(),
                                }
                            }
                        }
                    }
                    Fields::Unit => quote_spanned! { name.span() =>
                        #resolver::#variant => {
                            out.as_mut_ptr().cast::<ArchivedTag>().write(ArchivedTag::#variant);
                        }
                    }
                }
            });

            let archived_repr = if let Some(ref repr_attr) = attributes.archived_repr {
                if let Repr::Int(int_repr) = repr_attr.repr {
                    int_repr
                } else {
                    return Err(Error::new(
                        repr_attr.span,
                        "enums may only be repr(i*) or repr(u*)",
                    ));
                }
            } else {
                match data.variants.len() {
                    0..=255 => IntRepr::U8,
                    256..=65_535 => IntRepr::U16,
                    65_536..=4_294_967_295 => IntRepr::U32,
                    4_294_967_296..=18_446_744_073_709_551_615 => IntRepr::U64,
                    _ => IntRepr::U128,
                }
            };

            let is_fieldless = data
                .variants
                .iter()
                .all(|v| matches!(v.fields, Fields::Unit));
            #[cfg(all(
                not(feature = "arbitrary_enum_discriminant"),
                any(feature = "archive_le", feature = "archive_be")
            ))]
            if !is_fieldless && !matches!(archived_repr, IntRepr::U8 | IntRepr::I8) {
                return Err(Error::new_spanned(
                    name,
                    "enums with variant data cannot have multibyte discriminants when using endian-aware features\nenabling the `arbitrary_enum_discriminant` feature will allow this behavior",
                ));
            }

            let archived_def = if attributes.archive_as.is_none() {
                let archived_variants = data.variants.iter().enumerate().map(|(i, v)| {
                    let variant = &v.ident;
                    let discriminant =
                        if is_fieldless || cfg!(feature = "arbitrary_enum_discriminant") {
                            Some(archived_repr.enum_discriminant(i))
                        } else {
                            None
                        };
                    match v.fields {
                        Fields::Named(ref fields) => {
                            let fields = fields.named.iter().map(|f| {
                                let name = &f.ident;
                                let ty = with_ty(f);
                                let vis = &f.vis;
                                let archive_attrs = field_archive_attrs(f);
                                quote_spanned! { f.span() =>
                                    #(#[#archive_attrs])*
                                    #vis #name: #rkyv_path::Archived<#ty>
                                }
                            });
                            quote_spanned! { variant.span() =>
                                #[allow(dead_code)]
                                #variant {
                                    #(#fields,)*
                                } #discriminant
                            }
                        }
                        Fields::Unnamed(ref fields) => {
                            let fields = fields.unnamed.iter().map(|f| {
                                let ty = with_ty(f);
                                let vis = &f.vis;
                                let archive_attrs = field_archive_attrs(f);
                                quote_spanned! { f.span() =>
                                    #(#[#archive_attrs])*
                                    #vis #rkyv_path::Archived<#ty>
                                }
                            });
                            quote_spanned! { variant.span() =>
                                #[allow(dead_code)]
                                #variant(#(#fields,)*) #discriminant
                            }
                        }
                        Fields::Unit => quote_spanned! { variant.span() =>
                            #[allow(dead_code)]
                            #variant #discriminant
                        },
                    }
                });

                Some(quote! {
                    #[automatically_derived]
                    #[doc = #archived_doc]
                    #(#archive_attrs)*
                    #archived_repr
                    #vis enum #archived_name #generics #archive_where {
                        #(#archived_variants,)*
                    }
                })
            } else {
                None
            };

            let archived_variant_tags = data.variants.iter().enumerate().map(|(i, v)| {
                let variant = &v.ident;
                let discriminant = archived_repr.enum_discriminant(i);
                quote_spanned! { variant.span() => #variant #discriminant }
            });

            let archived_variant_structs = data.variants.iter().map(|v| {
                let variant = &v.ident;
                let archived_variant_name = Ident::new(&format!("ArchivedVariant{}", variant.to_string()), v.span());
                match v.fields {
                    Fields::Named(ref fields) => {
                        let fields = fields.named.iter().map(|f| {
                            let name = &f.ident;
                            let ty = with_ty(f);
                            quote_spanned! { f.span() => #name: Archived<#ty> }
                        });
                        quote_spanned! { name.span() =>
                            #[repr(C)]
                            struct #archived_variant_name #generics #archive_where {
                                __tag: ArchivedTag,
                                #(#fields,)*
                                __phantom: PhantomData<#name #ty_generics>,
                            }
                        }
                    }
                    Fields::Unnamed(ref fields) => {
                        let fields = fields.unnamed.iter().map(|f| {
                            let ty = with_ty(f);
                            quote_spanned! { f.span() => Archived<#ty> }
                        });
                        quote_spanned! { name.span() =>
                            #[repr(C)]
                            struct #archived_variant_name #generics (ArchivedTag, #(#fields,)* PhantomData<#name #ty_generics>) #archive_where;
                        }
                    }
                    Fields::Unit => quote! {}
                }
            });

            let mut partial_eq_impl = None;
            let mut partial_ord_impl = None;
            if let Some((_, ref compares)) = attributes.compares {
                for compare in compares {
                    if compare.is_ident("PartialEq") {
                        let mut partial_eq_where = archive_where.clone();
                        for variant in data.variants.iter() {
                            match variant.fields {
                                Fields::Named(ref fields) => {
                                    for field in fields.named.iter().filter(|f| {
                                        !f.attrs.iter().any(|a| a.path.is_ident("omit_bounds"))
                                    }) {
                                        let ty = with_ty(field);
                                        partial_eq_where
                                            .predicates
                                            .push(parse_quote! { Archived<#ty>: PartialEq<#ty> });
                                    }
                                }
                                Fields::Unnamed(ref fields) => {
                                    for field in fields.unnamed.iter().filter(|f| {
                                        !f.attrs.iter().any(|a| a.path.is_ident("omit_bounds"))
                                    }) {
                                        let ty = with_ty(field);
                                        partial_eq_where
                                            .predicates
                                            .push(parse_quote! { Archived<#ty>: PartialEq<#ty> });
                                    }
                                }
                                Fields::Unit => (),
                            }
                        }

                        let variant_impls = data.variants.iter().map(|v| {
                            let variant = &v.ident;
                            match v.fields {
                                Fields::Named(ref fields) => {
                                    let field_names = fields.named.iter()
                                        .map(|f| &f.ident)
                                        .collect::<Vec<_>>();
                                    let self_bindings = fields.named.iter().map(|f| {
                                        f.ident.as_ref().map(|ident| {
                                            Ident::new(&format!("self_{}", ident.to_string()), ident.span())
                                        })
                                    }).collect::<Vec<_>>();
                                    let other_bindings = fields.named.iter().map(|f| {
                                        f.ident.as_ref().map(|ident| {
                                            Ident::new(&format!("other_{}", ident.to_string()), ident.span())
                                        })
                                    }).collect::<Vec<_>>();
                                    quote! {
                                        #name::#variant { #(#field_names: #self_bindings,)* } => match other {
                                            #archived_name::#variant { #(#field_names: #other_bindings,)* } => true #(&& #other_bindings.eq(#self_bindings))*,
                                            #[allow(unreachable_patterns)]
                                            _ => false,
                                        }
                                    }
                                }
                                Fields::Unnamed(ref fields) => {
                                    let self_bindings = fields.unnamed.iter().enumerate().map(|(i, f)| {
                                        Ident::new(&format!("self_{}", i), f.span())
                                    }).collect::<Vec<_>>();
                                    let other_bindings = fields.unnamed.iter().enumerate().map(|(i, f)| {
                                        Ident::new(&format!("other_{}", i), f.span())
                                    }).collect::<Vec<_>>();
                                    quote! {
                                        #name::#variant(#(#self_bindings,)*) => match other {
                                            #archived_name::#variant(#(#other_bindings,)*) => true #(&& #other_bindings.eq(#self_bindings))*,
                                            #[allow(unreachable_patterns)]
                                            _ => false,
                                        }
                                    }
                                }
                                Fields::Unit => quote! {
                                    #name::#variant => match other {
                                        #archived_name::#variant => true,
                                        #[allow(unreachable_patterns)]
                                        _ => false,
                                    }
                                }
                            }
                        });

                        partial_eq_impl = Some(quote! {
                            impl #impl_generics PartialEq<#archived_type> for #name #ty_generics #partial_eq_where {
                                #[inline]
                                fn eq(&self, other: &#archived_type) -> bool {
                                    match self {
                                        #(#variant_impls,)*
                                    }
                                }
                            }

                            impl #impl_generics PartialEq<#name #ty_generics> for #archived_type #partial_eq_where {
                                #[inline]
                                fn eq(&self, other: &#name #ty_generics) -> bool {
                                    other.eq(self)
                                }
                            }
                        });
                    } else if compare.is_ident("PartialOrd") {
                        let mut partial_ord_where = archive_where.clone();
                        for variant in data.variants.iter() {
                            match variant.fields {
                                Fields::Named(ref fields) => {
                                    for field in fields.named.iter().filter(|f| {
                                        !f.attrs.iter().any(|a| a.path.is_ident("omit_bounds"))
                                    }) {
                                        let ty = with_ty(field);
                                        partial_ord_where
                                            .predicates
                                            .push(parse_quote! { Archived<#ty>: PartialOrd<#ty> });
                                    }
                                }
                                Fields::Unnamed(ref fields) => {
                                    for field in fields.unnamed.iter().filter(|f| {
                                        !f.attrs.iter().any(|a| a.path.is_ident("omit_bounds"))
                                    }) {
                                        let ty = with_ty(field);
                                        partial_ord_where
                                            .predicates
                                            .push(parse_quote! { Archived<#ty>: PartialOrd<#ty> });
                                    }
                                }
                                Fields::Unit => (),
                            }
                        }

                        let self_disc = data.variants.iter().enumerate().map(|(i, v)| {
                            let variant = &v.ident;
                            match v.fields {
                                Fields::Named(_) => quote! {
                                    #name::#variant { .. } => #i
                                },
                                Fields::Unnamed(_) => quote! {
                                    #name::#variant ( .. ) => #i
                                },
                                Fields::Unit => quote! {
                                    #name::#variant => #i
                                },
                            }
                        });
                        let other_disc = data.variants.iter().enumerate().map(|(i, v)| {
                            let variant = &v.ident;
                            match v.fields {
                                Fields::Named(_) => quote! {
                                    #archived_name::#variant { .. } => #i
                                },
                                Fields::Unnamed(_) => quote! {
                                    #archived_name::#variant ( .. ) => #i
                                },
                                Fields::Unit => quote! {
                                    #archived_name::#variant => #i
                                },
                            }
                        });

                        let variant_impls = data.variants.iter().map(|v| {
                            let variant = &v.ident;
                            match v.fields {
                                Fields::Named(ref fields) => {
                                    let field_names = fields.named.iter()
                                        .map(|f| &f.ident)
                                        .collect::<Vec<_>>();
                                    let self_bindings = fields.named.iter().map(|f| {
                                        f.ident.as_ref().map(|ident| {
                                            Ident::new(&format!("self_{}", ident.to_string()), ident.span())
                                        })
                                    }).collect::<Vec<_>>();
                                    let other_bindings = fields.named.iter().map(|f| {
                                        f.ident.as_ref().map(|ident| {
                                            Ident::new(&format!("other_{}", ident.to_string()), ident.span())
                                        })
                                    }).collect::<Vec<_>>();
                                    quote! {
                                        #name::#variant { #(#field_names: #self_bindings,)* } => match other {
                                            #archived_name::#variant { #(#field_names: #other_bindings,)* } => {
                                                #(
                                                    match #other_bindings.partial_cmp(#self_bindings) {
                                                        Some(::core::cmp::Ordering::Equal) => (),
                                                        cmp => return cmp,
                                                    }
                                                )*
                                                Some(::core::cmp::Ordering::Equal)
                                            }
                                            #[allow(unreachable_patterns)]
                                            _ => unsafe { ::core::hint::unreachable_unchecked() },
                                        }
                                    }
                                }
                                Fields::Unnamed(ref fields) => {
                                    let self_bindings = fields.unnamed.iter().enumerate().map(|(i, f)| {
                                        Ident::new(&format!("self_{}", i), f.span())
                                    }).collect::<Vec<_>>();
                                    let other_bindings = fields.unnamed.iter().enumerate().map(|(i, f)| {
                                        Ident::new(&format!("other_{}", i), f.span())
                                    }).collect::<Vec<_>>();
                                    quote! {
                                        #name::#variant(#(#self_bindings,)*) => match other {
                                            #archived_name::#variant(#(#other_bindings,)*) => {
                                                #(
                                                    match #other_bindings.partial_cmp(#self_bindings) {
                                                        Some(::core::cmp::Ordering::Equal) => (),
                                                        cmp => return cmp,
                                                    }
                                                )*
                                                Some(::core::cmp::Ordering::Equal)
                                            }
                                            #[allow(unreachable_patterns)]
                                            _ => unsafe { ::core::hint::unreachable_unchecked() },
                                        }
                                    }
                                }
                                Fields::Unit => quote! {
                                    #name::#variant => match other {
                                        #archived_name::#variant => Some(::core::cmp::Ordering::Equal),
                                        #[allow(unreachable_patterns)]
                                        _ => unsafe { ::core::hint::unreachable_unchecked() },
                                    }
                                }
                            }
                        });

                        partial_ord_impl = Some(quote! {
                            impl #impl_generics PartialOrd<#archived_type> for #name #ty_generics #partial_ord_where {
                                #[inline]
                                fn partial_cmp(&self, other: &#archived_type) -> Option<::core::cmp::Ordering> {
                                    let self_disc = match self { #(#self_disc,)* };
                                    let other_disc = match other { #(#other_disc,)* };
                                    if self_disc == other_disc {
                                        match self {
                                            #(#variant_impls,)*
                                        }
                                    } else {
                                        self_disc.partial_cmp(&other_disc)
                                    }
                                }
                            }

                            impl #impl_generics PartialOrd<#name #ty_generics> for #archived_type #partial_ord_where {
                                #[inline]
                                fn partial_cmp(&self, other: &#name #ty_generics) -> Option<::core::cmp::Ordering> {
                                    match other.partial_cmp(self) {
                                        Some(::core::cmp::Ordering::Less) => Some(::core::cmp::Ordering::Greater),
                                        Some(::core::cmp::Ordering::Greater) => Some(::core::cmp::Ordering::Less),
                                        cmp => cmp,
                                    }
                                }
                            }
                        });
                    } else {
                        return Err(Error::new_spanned(compare, "unrecognized compare argument, supported compares are PartialEq (PartialOrd is not supported for enums)"));
                    }
                }
            }

            let copy_safe_impl = if cfg!(feature = "copy") && attributes.copy_safe.is_some() {
                let mut copy_safe_where = where_clause.clone();
                for variant in data.variants.iter() {
                    match variant.fields {
                        Fields::Named(ref fields) => {
                            for field in fields
                                .named
                                .iter()
                                .filter(|f| !f.attrs.iter().any(|a| a.path.is_ident("omit_bounds")))
                            {
                                let ty = with_ty(field);
                                copy_safe_where
                                    .predicates
                                    .push(parse_quote! { #ty: #rkyv_path::copy::ArchiveCopySafe });
                            }
                        }
                        Fields::Unnamed(ref fields) => {
                            for field in fields
                                .unnamed
                                .iter()
                                .filter(|f| !f.attrs.iter().any(|a| a.path.is_ident("omit_bounds")))
                            {
                                let ty = with_ty(field);
                                copy_safe_where
                                    .predicates
                                    .push(parse_quote! { #ty: #rkyv_path::copy::ArchiveCopySafe });
                            }
                        }
                        Fields::Unit => (),
                    }
                }

                Some(quote! {
                    unsafe impl #impl_generics #rkyv_path::copy::ArchiveCopySafe for #name #ty_generics #copy_safe_where {}
                })
            } else {
                None
            };

            (
                quote! {
                    #archived_def

                    #[automatically_derived]
                    #[doc = #resolver_doc]
                    #vis enum #resolver #generics #archive_where {
                        #(#resolver_variants,)*
                    }
                },
                quote! {
                    #archived_repr
                    enum ArchivedTag {
                        #(#archived_variant_tags,)*
                    }

                    #(#archived_variant_structs)*

                    impl #impl_generics Archive for #name #ty_generics #archive_where {
                        type Archived = #archived_type;
                        type Resolver = #resolver #ty_generics;

                        // Some resolvers will be (), this allow is to prevent clippy from complaining
                        #[allow(clippy::unit_arg)]
                        #[inline]
                        unsafe fn resolve(&self, pos: usize, resolver: Self::Resolver, out: &mut MaybeUninit<Self::Archived>) {
                            match resolver {
                                #(#resolve_arms,)*
                            }
                        }
                    }

                    #partial_eq_impl
                    #partial_ord_impl
                    #copy_safe_impl
                },
            )
        }
        Data::Union(_) => {
            return Err(Error::new_spanned(
                input,
                "Archive cannot be derived for unions",
            ))
        }
    };

    Ok(quote! {
        #archive_types

        #[automatically_derived]
        const _: () = {
            use ::core::{marker::PhantomData, mem::MaybeUninit};
            use #rkyv_path::{out_field, Archive, Archived};

            #archive_impls
        };
    })
}
