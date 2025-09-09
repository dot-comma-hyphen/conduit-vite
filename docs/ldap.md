# LDAP Authentication

Conduit can be configured to use an LDAP server for user authentication. When a user logs in with their LDAP credentials for the first time, a new Matrix account will be automatically provisioned for them.

## Configuration

To enable LDAP authentication, add the following section to your `conduit.toml` file:

```toml
[ldap]
enabled = true
uri = "ldap://ldap.example.com:389"
bind_dn = "cn=readonly,dc=example,dc=com"
bind_password = "password"
base_dn = "ou=users,dc=example,dc=com"
user_filter = "(uid=%u)"
attribute_mapping = { localpart = "uid", displayname = "cn", email = "mail" }
```

### Options

- `enabled`: Set to `true` to enable LDAP authentication.
- `uri`: The URI of the LDAP server.
- `bind_dn`: The Distinguished Name (DN) of the user to bind to the LDAP server for searching.
- `bind_password`: The password for the `bind_dn`.
- `base_dn`: The base DN to search for users in.
- `user_filter`: The filter to use when searching for a user. `%u` will be replaced with the username provided by the user.
- `attribute_mapping`: A map of Conduit user attributes to LDAP attributes.
    - `localpart`: The LDAP attribute to use for the user's localpart (username).
    - `displayname`: The LDAP attribute to use for the user's display name.
    - `email`: The LDAP attribute to use for the user's email address.

## Login Flow

When a user attempts to log in with a username and password:

1.  Conduit will first attempt to authenticate the user against the LDAP server.
2.  If the user is found in LDAP and the password is correct, Conduit will check if a local Matrix user with the same localpart exists.
3.  If the user does not exist locally, a new Matrix account will be created with the attributes mapped from LDAP.
4.  The user is then logged in.
5.  If LDAP authentication fails (user not found or incorrect password), Conduit will fall back to the normal password-based authentication against its local database.
