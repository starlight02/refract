//! 进程内 HTTP 测试夹具。
//!
//! 走 `build_app(state).finish().call(())`，不再依赖任何框架自带的 test client。

use xitca_web::body::RequestBody;
use xitca_web::bytes::Bytes;
use xitca_web::http::{
    HeaderMap, HeaderName, HeaderValue, Method, Request, RequestExt, StatusCode, header,
};

use crate::state::AppState;

/// 一次进程内请求。
pub struct TestRequest {
    method: Method,
    uri: String,
    headers: HeaderMap,
    body: Bytes,
}

impl TestRequest {
    fn new(method: Method, path: &str) -> Self {
        Self {
            method,
            uri: path.to_owned(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
        }
    }

    /// `GET path`。
    pub fn get(path: &str) -> Self {
        Self::new(Method::GET, path)
    }

    /// `POST path`。
    pub fn post(path: &str) -> Self {
        Self::new(Method::POST, path)
    }

    /// `PUT path`。
    pub fn put(path: &str) -> Self {
        Self::new(Method::PUT, path)
    }

    /// `DELETE path`。
    pub fn delete(path: &str) -> Self {
        Self::new(Method::DELETE, path)
    }

    /// 覆盖 HTTP 方法。
    pub fn method(mut self, method: Method) -> Self {
        self.method = method;
        self
    }

    /// 追加一个请求头。
    pub fn header(mut self, name: impl AsHeaderName, value: impl IntoHeaderValue) -> Self {
        self.headers
            .insert(name.into_header_name(), value.into_header_value());
        self
    }

    /// 写入 JSON body，并设置 `content-type: application/json`。
    pub fn json(mut self, value: &impl serde::Serialize) -> Self {
        self.body = Bytes::from(serde_json::to_vec(value).expect("test json"));
        self.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        self
    }

    /// 写入原始 body。
    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    /// 对完整应用发请求并收齐响应体。
    pub async fn send(self, state: AppState) -> TestResponse {
        let mut builder = Request::builder().method(self.method).uri(&self.uri);
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        let ext = RequestExt::<RequestBody>::default().map_body(|_| RequestBody::from(self.body));
        let request = builder.body(ext).expect("test request");
        let (status, headers, body) = crate::dispatch_test(state, request).await;
        TestResponse {
            status,
            headers,
            body,
        }
    }
}

/// 收齐后的响应。
pub struct TestResponse {
    /// 状态码。
    pub status: StatusCode,
    /// 响应头。
    pub headers: HeaderMap,
    /// 完整响应体。
    pub body: Bytes,
}

impl TestResponse {
    /// 状态码。
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// 响应头。
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// 响应体字节。
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// 把 body 解析成 JSON。
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!(
                "response is not json ({error}): {}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }
}

/// 接受 `&str` / `HeaderName`。
pub trait AsHeaderName {
    /// 转成拥有的 `HeaderName`。
    fn into_header_name(self) -> HeaderName;
}

impl AsHeaderName for HeaderName {
    fn into_header_name(self) -> HeaderName {
        self
    }
}

impl AsHeaderName for &str {
    fn into_header_name(self) -> HeaderName {
        HeaderName::try_from(self).expect("invalid test header name")
    }
}

/// 接受 `&str` / `HeaderValue`。
pub trait IntoHeaderValue {
    /// 转成 `HeaderValue`。
    fn into_header_value(self) -> HeaderValue;
}

impl IntoHeaderValue for HeaderValue {
    fn into_header_value(self) -> HeaderValue {
        self
    }
}

impl IntoHeaderValue for &str {
    fn into_header_value(self) -> HeaderValue {
        HeaderValue::try_from(self).expect("invalid test header value")
    }
}

impl IntoHeaderValue for String {
    fn into_header_value(self) -> HeaderValue {
        HeaderValue::try_from(self).expect("invalid test header value")
    }
}
