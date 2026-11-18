//! A fake identity provider: JWKS, discovery, device authorization and token renewal.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use serde::Serialize;
use serde_json::json;
use tokio::net::TcpListener;

use super::TestBody;
use crate::common::{serve_http1, text};

/// Signing key the fake provider publishes and uses.
const RSA_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDgBveRZuyyZLNq
PUk4A0Z1eucHnxQMexky274cc/0K+gMY7BgoQmjmLfxQ4hor2dVYrdwMnK8ELQ9m
U6rqbN/iFyUAoG7MNuT1jCMAgDMXrCC0OspzGW/Fac7j7E44tloUdS6B+24p3Gro
MGohxT0UaO+t9jx32Kf4kTNvf3UgaMurkoT2F75oSXoqqCl+5ONiE6vUMxo8Ny4w
YQs6WdJVNOH9BU3NVF/pDMOtNUy6Ft3eCP0efomLjjyaoeFAQIsE4KnxADUr9b3s
/RkQBgYakwA4yR39Z9w/ggpn/U7OMzNjsChXF16wOaToteGV78mIAbSI7DiaACLg
xJj+HBeRAgMBAAECggEASK/6A6NxiTBDRmU8LIBjjBqke0SSLLqEIoxJ0tGPeNBF
0QBJlkZ14rlo9JVoCHJVVsCSdUUhRCmapYjgG8/XlHQN/XYQfRISl9kwQilWaofe
1Jt+tHm8WkFKb4lsl9oSwDCFNodt5wctaMjMpshoWQbzPW53kCsr1OrH1ZV1W14J
rzflyYQHCQs5eN6KqdLPukKqSBCwqfuuaGc+WwxluUHBSDuTotIXqq0s1tsZ17PO
uiekWHhT6M5bYdCKSaHwcoBS/az5sqekSKlYuxNRv5DzEohpjMPeR6+O3WnZeqgQ
1fUHGOXzRRq/lwuecWtVZCinxPerEjt2zJf5C1FvQQKBgQD5VwaOANfnZvmT0kXW
W6EuSBWNH0qf/VGFm+7g1ut4cKgG6QFqNl7PsZkTnrpTT10I0x9SzkRXEazA5mv5
WLUtqwgAAYTpvaNW2AuzpnSQSmBuEkGeufu6SoNGUxhKB2r/h2cvJy8/MJyM4J42
zOFn1DE2gmfPL2z03HQwu4ZbSwKBgQDmAtqwP0rQGxjHSH4FN1P3NR/GWtHVzqPf
PIpa5w7MoTvTcg30j2C602uzFw2N5decoLL6QHjYHu8b93x0NNU2McVVyC+yTDYA
yzIaCtf/5DK7VJ8LbstZfKFDcaKsHUCl6z1CN6W2k6utS5G52IqLH4QRmwnmsliD
QNA+Ol5TEwKBgBgRVGqknjKR+AlSxLb6RUaoYpwqpxNmalYRRS+izzvi8/UmzKIp
+WDm7bW9HpMoF/zgFCf0DRiydhBwrX92Fpb/6VfGzcOaMIgTdp2ytZGv/DrjvD3M
CgZjZuSE5NVQff2IXmdoyuC4moPmsp7ri95v1whB8i9HjqbcGMMTdtadAoGBAOUg
cMNjuV4HsC5M66S8kftPw2Bv9lobqCjogRa+14JHF3ayRm6aKYzSsZXM4Ca7kIap
msEqWndIkX71kdedDcwX1nF2ovTlcGJoCHfi13tE0rjEOk9ERnPVcDvkRl1YIp9l
8Dc3ge+XuUw/XGKa8KBYe9Kh9EII5Z2Sgh+LO41bAoGAF/xwjCq0VXQSthaF/6sA
CVRwlQ0SwyJTJjhdjOt5eRTvkppQF4pzLFenvwGB0OpRLc5gCzMU0c8GS4ZHxopl
yJvqHv/euDVENiC60A76q0JIxWPjGsu5h1Ypq02YrL1lP+2Qu0Uo9SZ19G1T6Vij
y1YGKDtUkPciDM45+D6GKXg=
-----END PRIVATE KEY-----";

const RSA_N: &str = "4Ab3kWbssmSzaj1JOANGdXrnB58UDHsZMtu-HHP9CvoDGOwYKEJo5i38UOIaK9nVWK3cDJyvBC0PZlOq6mzf4hclAKBuzDbk9YwjAIAzF6wgtDrKcxlvxWnO4-xOOLZaFHUugftuKdxq6DBqIcU9FGjvrfY8d9in-JEzb391IGjLq5KE9he-aEl6KqgpfuTjYhOr1DMaPDcuMGELOlnSVTTh_QVNzVRf6QzDrTVMuhbd3gj9Hn6Ji448mqHhQECLBOCp8QA1K_W97P0ZEAYGGpMAOMkd_WfcP4IKZ_1OzjMzY7AoVxdesDmk6LXhle_JiAG0iOw4mgAi4MSY_hwXkQ";

const RSA_E: &str = "AQAB";

#[derive(Serialize)]
struct Claims {
    iss: String,
    sub: String,
    aud: String,
    exp: u64,
    iat: u64,
    azp: &'static str,
    scope: String,
    preferred_username: String,
}

/// A running fake provider, issuing tokens for one account.
pub struct FakeIdp {
    /// Address of the HTTP endpoints.
    pub addr: SocketAddr,
    /// Account name carried by every token.
    pub username: String,
    /// Audience every token carries.
    pub audience: String,
}

impl FakeIdp {
    /// Mints an access token with explicit audience and scope, for rejection tests.
    #[must_use]
    pub fn token(&self, audience: &str, scope: &str) -> String {
        access_token(
            &format!("http://{}", self.addr),
            &self.username,
            audience,
            scope,
        )
    }

    /// Starts the provider for `username`, minting tokens for `audience`.
    pub async fn start(username: &str, audience: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let username = username.to_owned();
        let audience = audience.to_owned();
        let counter = Arc::new(AtomicU32::new(0));
        let handler = {
            let base = base.clone();
            let username = username.clone();
            let audience = audience.clone();
            move |request: Request<Incoming>| {
                let base = base.clone();
                let username = username.clone();
                let audience = audience.clone();
                let counter = counter.clone();
                async move {
                    let (parts, body) = request.into_parts();
                    let bytes = body
                        .collect()
                        .await
                        .map_or_else(|_| Default::default(), |b| b.to_bytes());
                    let form = String::from_utf8_lossy(&bytes).to_string();
                    let reply = match (parts.method, parts.uri.path()) {
                        (Method::GET, "/.well-known/openid-configuration") => json_reply(json!({
                            "issuer": base,
                            "jwks_uri": format!("{base}/jwks"),
                            "device_authorization_endpoint": format!("{base}/device"),
                            "token_endpoint": format!("{base}/token"),
                        })),
                        (Method::GET, "/jwks") => json_reply(json!({
                            "keys": [{
                                "kty": "RSA",
                                "alg": "RS256",
                                "use": "sig",
                                "kid": "test-rsa",
                                "n": RSA_N,
                                "e": RSA_E,
                            }]
                        })),
                        (Method::POST, "/device") => {
                            let index = counter.fetch_add(1, Ordering::Relaxed);
                            json_reply(json!({
                                "device_code": format!("device-{index}"),
                                "user_code": "ABCD-EFGH",
                                "verification_uri_complete": format!("{base}/device?code=ABCD-EFGH"),
                                "interval": 1,
                                "expires_in": 60,
                            }))
                        }
                        (Method::POST, "/token") if form.contains("device_code") => {
                            json_reply(json!({
                                "access_token": access_token(
                                    &base,
                                    &username,
                                    &audience,
                                    "openid profile offline_access tunnel:serve",
                                ),
                                "refresh_token": format!("refresh-{username}"),
                                "token_type": "Bearer",
                            }))
                        }
                        (Method::POST, "/token") if form.contains("refresh_token") => {
                            json_reply(json!({
                                "access_token": access_token(
                                    &base,
                                    &username,
                                    &audience,
                                    "openid profile offline_access tunnel:serve",
                                ),
                                "refresh_token": format!("refresh-{username}"),
                                "token_type": "Bearer",
                            }))
                        }
                        _ => Response::builder()
                            .status(StatusCode::NOT_FOUND)
                            .body(Full::new(bytes::Bytes::new()).boxed())
                            .unwrap(),
                    };
                    Ok::<Response<TestBody>, Infallible>(reply)
                }
            }
        };
        tokio::spawn(serve_http1(listener, handler));
        Self {
            addr,
            username,
            audience,
        }
    }
}

fn access_token(issuer: &str, username: &str, audience: &str, scope: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let claims = Claims {
        iss: issuer.to_owned(),
        sub: format!("user-{username}"),
        aud: audience.to_owned(),
        exp: now + 3600,
        iat: now,
        azp: "reverse-agent",
        scope: scope.to_owned(),
        preferred_username: username.to_owned(),
    };
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("test-rsa".to_owned());
    jsonwebtoken::encode(
        &header,
        &claims,
        &jsonwebtoken::EncodingKey::from_rsa_pem(RSA_PEM.as_bytes()).unwrap(),
    )
    .unwrap()
}

fn json_reply(body: serde_json::Value) -> Response<TestBody> {
    text(
        StatusCode::OK,
        "application/json",
        body.to_string().into_bytes(),
    )
}
