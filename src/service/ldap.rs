use crate::{Result, services};
use ldap3::{LdapConn, Scope, SearchEntry};

#[derive(Debug)]
pub struct LdapUser {
    pub dn: String,
    pub localpart: String,
    pub displayname: String,
    pub email: String,
}

pub struct Service;

impl Service {
    pub fn build() -> Result<Self> {
        Ok(Self)
    }

    pub fn find_ldap_user(&self, username: &str) -> Result<LdapUser> {
        let ldap_config = &services().globals.config.ldap;
        let mut ldap = LdapConn::new(&ldap_config.uri)?;
        ldap.simple_bind(&ldap_config.bind_dn, &ldap_config.bind_password)?;

        let filter = ldap_config.user_filter.replace("%u", username);
        let (rs, _res) = ldap
            .search(
                &ldap_config.base_dn,
                Scope::Subtree,
                &filter,
                &[
                    ldap_config.attribute_mapping.get("localpart").unwrap(),
                    ldap_config.attribute_mapping.get("displayname").unwrap(),
                    ldap_config.attribute_mapping.get("email").unwrap(),
                ],
            )?
            .success()?;

        if rs.len() != 1 {
            return Err(crate::Error::BadRequest(
                ruma::api::client::error::ErrorKind::NotFound,
                "User not found or multiple users found",
            ));
        }

        let entry = SearchEntry::construct(rs.into_iter().next().unwrap());
        let dn = entry.dn.clone();
        let localpart_attr = ldap_config.attribute_mapping.get("localpart").unwrap();
        let displayname_attr = ldap_config.attribute_mapping.get("displayname").unwrap();
        let email_attr = ldap_config.attribute_mapping.get("email").unwrap();

        let localpart = entry
            .attrs
            .get(localpart_attr)
            .and_then(|vals| vals.get(0))
            .ok_or_else(|| {
                crate::Error::bad_config("LDAP attribute for localpart not found")
            })?
            .to_owned();

        let displayname = entry
            .attrs
            .get(displayname_attr)
            .and_then(|vals| vals.get(0))
            .ok_or_else(|| {
                crate::Error::bad_config("LDAP attribute for displayname not found")
            })?
            .to_owned();

        let email = entry
            .attrs
            .get(email_attr)
            .and_then(|vals| vals.get(0))
            .ok_or_else(|| {
                crate::Error::bad_config("LDAP attribute for email not found")
            })?
            .to_owned();

        Ok(LdapUser {
            dn,
            localpart,
            displayname,
            email,
        })
    }
}