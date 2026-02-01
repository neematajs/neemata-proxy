use crate::errors;
use napi::bindgen_prelude::*;
use napi_derive::napi;

#[napi(object)]
pub struct ProxyOptions {
    pub listen: String,
    pub tls: Option<ListenerTlsOptions>,
    pub applications: Vec<ApplicationOptions>,
    /// Health check interval in milliseconds. Defaults to 5000ms (5 seconds).
    pub health_check_interval_ms: Option<u32>,
}

#[napi(object)]
pub struct ListenerTlsOptions {
    pub cert_path: String,
    pub key_path: String,
    pub enable_h2: Option<bool>,
}

#[napi(object)]
pub struct ApplicationOptions {
    pub name: String,
    pub routing: RoutingOptions,
    pub sni: Option<String>,
}

#[napi(object)]
pub struct RoutingOptions {
    #[napi(ts_type = " 'subdomain' | 'path' ")]
    pub r#type: Option<String>,
    pub name: Option<String>,
    pub default: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct ProxyOptionsParsed {
    pub listen: String,
    pub tls: Option<ListenerTlsOptionsParsed>,
    pub applications: Vec<ApplicationOptionsParsed>,
    /// Health check interval in milliseconds.
    pub health_check_interval_ms: u32,
}

#[derive(Debug, Clone)]
pub struct ListenerTlsOptionsParsed {
    pub cert_path: String,
    pub key_path: String,
    pub enable_h2: bool,
}

#[derive(Debug, Clone)]
pub struct ApplicationOptionsParsed {
    pub name: String,
    pub routing: ApplicationRoutingParsed,
    pub sni: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ApplicationRoutingParsed {
    Subdomain { name: String },
    Path { name: String },
    Default,
}

pub fn parse_proxy_options(env: &Env, options: ProxyOptions) -> Result<ProxyOptionsParsed> {
    let listen = options.listen;

    let tls = options.tls.map(|tls| ListenerTlsOptionsParsed {
        cert_path: tls.cert_path,
        key_path: tls.key_path,
        enable_h2: tls.enable_h2.unwrap_or(false),
    });

    let mut applications = Vec::with_capacity(options.applications.len());
    for app in options.applications {
        applications.push(parse_application_options(env, app)?);
    }

    validate_applications(env, &applications)?;

    Ok(ProxyOptionsParsed {
        listen,
        tls,
        applications,
        health_check_interval_ms: options.health_check_interval_ms.unwrap_or(5000),
    })
}

fn parse_application_options(
    env: &Env,
    app: ApplicationOptions,
) -> Result<ApplicationOptionsParsed> {
    let name = app.name;
    let sni = app.sni;
    let routing = parse_routing(env, app.routing)?;
    Ok(ApplicationOptionsParsed { name, routing, sni })
}

fn parse_routing(env: &Env, routing: RoutingOptions) -> Result<ApplicationRoutingParsed> {
    if routing.default.unwrap_or(false) {
        return Ok(ApplicationRoutingParsed::Default);
    }

    let Some(routing_type) = routing.r#type else {
        return errors::throw_type_error(
            env,
            errors::codes::INVALID_APPLICATION_OPTIONS,
            "ApplicationOptions.routing.type must be set unless routing.default=true",
        );
    };
    let Some(routing_name) = routing.name else {
        return errors::throw_type_error(
            env,
            errors::codes::INVALID_APPLICATION_OPTIONS,
            "ApplicationOptions.routing.name must be set unless routing.default=true",
        );
    };

    match routing_type.as_str() {
        "subdomain" => Ok(ApplicationRoutingParsed::Subdomain { name: routing_name }),
        "path" => Ok(ApplicationRoutingParsed::Path { name: routing_name }),
        other => errors::throw_type_error(
            env,
            errors::codes::INVALID_APPLICATION_OPTIONS,
            format!(
                "Unknown routing.type '{other}'. Expected 'subdomain' or 'path', or use routing.default=true"
            ),
        ),
    }
}

fn validate_applications(env: &Env, applications: &[ApplicationOptionsParsed]) -> Result<()> {
    let mut default_count = 0usize;

    for app in applications {
        match &app.routing {
            ApplicationRoutingParsed::Default => {
                default_count += 1;
            }
            ApplicationRoutingParsed::Subdomain { name } => {
                if name.is_empty() {
                    return errors::throw_type_error(
                        env,
                        errors::codes::INVALID_APPLICATION_OPTIONS,
                        "subdomain routing name must be non-empty",
                    );
                }
            }
            ApplicationRoutingParsed::Path { name } => {
                if name.is_empty() {
                    return errors::throw_type_error(
                        env,
                        errors::codes::INVALID_APPLICATION_OPTIONS,
                        "path routing name must be non-empty",
                    );
                }
                if name.contains('/') {
                    return errors::throw_type_error(
                        env,
                        errors::codes::INVALID_APPLICATION_OPTIONS,
                        "path routing name must not contain '/'",
                    );
                }
            }
        }
    }

    if default_count > 1 {
        return errors::throw_type_error(
            env,
            errors::codes::INVALID_APPLICATION_OPTIONS,
            "At most one application may have routing.default=true",
        );
    }

    Ok(())
}
