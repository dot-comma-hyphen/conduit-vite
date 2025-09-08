use serde::Deserialize;
use std::collections::HashMap;

#[derive(Clone, Debug, Deserialize, Default)]
pub struct LdapConfig {
    #[serde(default = "default_ldap_enabled")]
    pub enabled: bool,
    pub uri: String,
    pub bind_dn: String,
    pub bind_password: String,
    pub base_dn: String,
    #[serde(default = "default_user_filter")]
    pub user_filter: String,
    #[serde(default = "default_attribute_mapping")]
    pub attribute_mapping: HashMap<String, String>,
}

fn default_ldap_enabled() -> bool {
    false
}

fn default_user_filter() -> String {
    "(uid=%u)".to_owned()
}

fn default_attribute_mapping() -> HashMap<String, String> {
    let mut map = HashMap::new();
    map.insert("localpart".to_owned(), "uid".to_owned());
    map.insert("displayname".to_owned(), "cn".to_owned());
    map.insert("email".to_owned(), "mail".to_owned());
    map
}
