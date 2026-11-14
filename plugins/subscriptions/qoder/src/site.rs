//! Site selection and Qoder CN enterprise VPC host rules.

use hofund_plugin_api::PluginError;

use crate::Endpoints;
use crate::creds::{Credential, Site};

pub(crate) const VPC_DOMAIN: &str = "vpc.qoder.com.cn";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub web: String,
    pub openapi: String,
    pub chat: String,
    pub vpc: String,
}

impl Endpoints {
    pub(crate) fn resolve(&self, credential: &Credential) -> Result<Resolved, PluginError> {
        self.resolve_for(credential.site, &credential.vpc)
    }

    pub(crate) fn resolve_for(&self, site: Site, instance: &str) -> Result<Resolved, PluginError> {
        if instance.is_empty() {
            return Ok(Resolved {
                web: self.web_for(site).to_owned(),
                openapi: self.openapi_for(site).to_owned(),
                chat: self.chat_for(site).to_owned(),
                vpc: String::new(),
            });
        }
        if site != Site::China {
            return Err(PluginError::auth(
                "Qoder VPC instances exist only on Qoder CN; sign in again",
            ));
        }
        let Some(instance) = vpc_instance(instance) else {
            return Err(PluginError::auth(format!(
                "the account's VPC instance {instance:?} is not valid; sign in again"
            )));
        };
        Ok(Resolved {
            web: format!("https://{instance}.{VPC_DOMAIN}"),
            openapi: format!("https://{instance}-openapi.{VPC_DOMAIN}"),
            chat: format!("https://{instance}-gateway.{VPC_DOMAIN}"),
            vpc: instance.to_owned(),
        })
    }
}

pub(crate) fn vpc_instance(input: &str) -> Option<String> {
    let mut entry = input.trim().to_ascii_lowercase();
    if entry.is_empty() {
        return None;
    }
    if entry.contains("://") {
        let url = hofund_plugin_sdk::url::Url::parse(&entry).ok()?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(url.path(), "" | "/")
        {
            return None;
        }
        entry = url.host_str()?.to_owned();
    }
    let octets: Vec<&str> = entry.split('.').collect();
    if octets.len() == 4
        && octets
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    let name = if let Some(prefix) = entry.strip_suffix(&format!(".{VPC_DOMAIN}")) {
        prefix
            .strip_suffix("-openapi")
            .or_else(|| prefix.strip_suffix("-gateway"))
            .unwrap_or(prefix)
    } else {
        if entry.contains('.') {
            return None;
        }
        entry.as_str()
    };
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    valid.then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::vpc_instance;

    #[test]
    fn r54b_vpc_names_and_hosts_follow_the_reference() {
        for valid in [
            "acme",
            " ACME ",
            "acme.vpc.qoder.com.cn",
            "acme-openapi.vpc.qoder.com.cn",
            "acme-gateway.vpc.qoder.com.cn",
            "https://acme.vpc.qoder.com.cn",
            "https://acme.vpc.qoder.com.cn/",
        ] {
            assert_eq!(vpc_instance(valid).as_deref(), Some("acme"), "{valid}");
        }
        for invalid in [
            "",
            "http://acme.vpc.qoder.com.cn",
            "https://acme.vpc.qoder.com.cn/x",
            "https://acme.vpc.qoder.com.cn?a=1",
            "https://u:p@acme.vpc.qoder.com.cn",
            "https://acme.vpc.qoder.com.cn:8443",
            "10.0.0.1",
            "acme.example.com",
            "-acme",
            "acme-",
            "ac_me",
            "a.b.vpc.qoder.com.cn",
        ] {
            assert_eq!(vpc_instance(invalid), None, "{invalid}");
        }
    }
}
