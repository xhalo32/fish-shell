//! This file routes the [`Provider`] type to point to the default YAML history or `history_impl::Provider` if the `external-history` feature is enabled.

#[cfg(not(feature = "external-history"))]
pub type Provider = super::yaml::YAMLHistory;

#[cfg(feature = "external-history")]
pub type Provider = history_impl::Provider;
