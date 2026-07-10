use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PropertyType {
    String,
    Int,
    Bool,
    SecurityGroup,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigProperty {
    pub key:           String,
    pub name:          String,
    pub description:   String,
    pub property_type: PropertyType,
    /// Value used when the key is absent from the DB config table.
    pub default_value: Option<String>,
}

/// A named group of related [`ConfigProperty`] items, shown as a distinct
/// section in the Config UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSet {
    pub name:        String,
    pub description: String,
    pub properties:  Vec<ConfigProperty>,
}
