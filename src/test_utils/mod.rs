//! Utilities for setting up mock clients and test servers with similar features to `preroll::main!`

#![allow(clippy::unwrap_used)]

use std::convert::TryInto;
use std::env;
use std::fmt::Debug;
use std::sync::Arc;

use cfg_if::cfg_if;
use surf::{Client, StatusCode, Url};
use tide::{http, Server};

use crate::logging::{log_format_json, log_format_pretty};
use crate::middleware::{JsonErrorMiddleware, LogMiddleware, RequestIdMiddleware};

use crate::middleware::json_error::JsonError;

#[cfg(feature = "honeycomb")]
use tracing_subscriber::Registry;

cfg_if! {
    if #[cfg(feature = "postgres")] {
        use async_std::sync::RwLock;
        use sqlx::postgres::{PgConnectOptions, PgPoolOptions, Postgres};
        use sqlx::ConnectOptions;
        use tide::{Middleware, Next, Request};

        use crate::middleware::postgres::{ConnectionWrap, ConnectionWrapInner};
    }
}

/// The result type to use for tests.
///
/// This is a `surf::Result<T>`.
pub type TestResult<T> = surf::Result<T>;

/// Creates a test application with routes and mocks set up,
/// and hands back a client which is already connected to the server.
///
/// ## Example:
/// ```
/// // use preroll::test_utils::{self, TestResult};
///
/// #[async_std::test]
/// async fn example_test() -> TestResult<()> {
///     let client = test_utils::create_app().await.unwrap();
///
///     // ... (test cases) ...
///
///     Ok(())
/// }
/// ```
pub async fn create_client<State, RoutesFn>(
    state: State,
    setup_routes_fn: RoutesFn,
) -> TestResult<Client>
where
    State: Send + Sync + 'static,
    RoutesFn: for<'s> Fn(&'s mut Server<Arc<State>>),
{
    let server = create_server(state, setup_routes_fn)?;

    let mut client = Client::with_http_client(server);
    client.set_base_url(Url::parse("http://localhost:8080")?); // Address not actually used.

    Ok(client)
}

/// Creates a test application with routes and mocks set up,
/// and hands back a client which is already connected to the server.
///
/// This function also hands back a postgres transaction connection which is
/// being used for the rest of the application, allowing easy rollback of everything.
///
/// ## Important!
///
/// The `RwLockWriteGuard` returned from `pg_conn.write().await` MUST be [dropped][] before running
/// the test cases, or else there will be a writer conflict and the test will hang indefinitely.
///
/// ## Example:
/// ```
/// // use preroll::test_utils::{self, TestResult};
///
/// #[async_std::test]
/// async fn example_test_with_postgres() -> TestResult<()> {
///     let (client, pg_conn) = test_utils::create_client_and_postgres().await.unwrap();
///
///     {
///         let mut pg_conn = pg_conn.write().await;
///
///         // ... (test setup) ...
///
///         // The RwLockWriteGuard here MUST be dropped before running the test cases,
///         // or else there is a writer conflict and the test hangs indefinitely.
///         //
///         // Note: this is done automatically at the end of the closure.
///         // We are still explicitly dropping so as to avoid accidently messing this up in the future.
///         std::mem::drop(pg_conn);
///     }
///
///     // ... (test cases) ...
///
///     Ok(())
/// }
/// ```
///
/// [dropped]: https://doc.rust-lang.org/reference/destructors.html
#[cfg(feature = "postgres")]
#[cfg_attr(feature = "docs", doc(cfg(feature = "postgres")))]
pub async fn create_client_and_postgres<State, RoutesFn>(
    state: State,
    setup_routes_fn: RoutesFn,
) -> TestResult<(Client, Arc<RwLock<ConnectionWrapInner<Postgres>>>)>
where
    State: Send + Sync + 'static,
    RoutesFn: Fn(&mut Server<Arc<State>>),
{
    let mut server = create_server(state, setup_routes_fn)?;

    // Fake PostgresConnectionMiddleware.
    //
    // We do this so that all connections within any test run can share the same Transaction and be rolled back on Drop.
    let mut connect_opts = PgConnectOptions::new()
        .host("localhost")
        .database("database_test");
    connect_opts.log_statements(log::LevelFilter::Debug);

    let pg_pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(connect_opts)
        .await?;

    let conn_wrap = Arc::new(RwLock::new(ConnectionWrapInner::Transacting(
        pg_pool.begin().await?,
    )));
    server.with(PostgresTestMiddleware(conn_wrap.clone()));

    let mut client = Client::with_http_client(server);
    client.set_base_url(Url::parse("http://localhost:8080")?); // Address not actually used.

    Ok((client, conn_wrap))
}

pub(crate) fn create_server<State, RoutesFn>(
    state: State,
    setup_routes_fn: RoutesFn,
) -> TestResult<Server<Arc<State>>>
where
    State: Send + Sync + 'static,
    RoutesFn: Fn(&mut Server<Arc<State>>),
{
    dotenv::dotenv().ok();

    let log_level: log::LevelFilter = env::var("LOGLEVEL")
        .map(|v| v.parse().expect("LOGLEVEL must be a valid log level."))
        .unwrap_or(log::LevelFilter::Off);

    let environment = env::var("ENVIRONMENT").unwrap_or_else(|_| "development".to_string());

    if environment.starts_with("prod") {
        // Like Production
        env_logger::builder()
            .format(log_format_json)
            .filter_level(log_level)
            .write_style(env_logger::WriteStyle::Never)
            .try_init()
            .ok();
    } else {
        // Like Development
        env_logger::builder()
            .format(log_format_pretty)
            .filter_level(log_level)
            .try_init()
            .ok();
    }

    cfg_if! {
        if #[cfg(feature = "honeycomb")] {
            let subscriber = Registry::default();
            // .with(tracing_subscriber::fmt::Layer::default()) // log to stdout
            tracing::subscriber::set_global_default(subscriber).ok();
        }
    }

    let mut server = tide::with_state(Arc::new(state));
    server.with(RequestIdMiddleware::new());
    server.with(LogMiddleware::new());
    server.with(JsonErrorMiddleware::new());

    server
        .at("/monitor/ping")
        .get(|_| async { Ok("preroll_test_utils") });

    setup_routes_fn(&mut server);

    Ok(server)
}

cfg_if! {
    if #[cfg(feature = "postgres")] {
        #[derive(Debug, Clone)]
        struct PostgresTestMiddleware(ConnectionWrap<Postgres>);

        #[tide::utils::async_trait]
        impl<State: Clone + Send + Sync + 'static> Middleware<State> for PostgresTestMiddleware {
            async fn handle(&self, mut req: Request<State>, next: Next<'_, State>) -> tide::Result {
                req.set_ext(self.0.clone());
                Ok(next.run(req).await)
            }
        }
    }
}

/// Creates a mock client directly connected to a server which is setup by the provided function.
///
/// ## Example:
/// ```
/// use preroll::test_utils;
/// use tide::Server;
///
/// fn setup_example_local_org_mocks(mock: &mut Server<()>) {
///     mock.at("hello-world").get(|_| async { Ok("Hello World!") });
/// }
///
/// #[async_std::main]
/// async fn main() {
///     let client = test_utils::mock_client("http://api.example_local.org/", setup_example_local_org_mocks);
///
///     let response = client
///         .get("http://api.example_local.org/hello-world")
///         .recv_string()
///         .await
///         .unwrap();
///
///     assert_eq!(response, "Hello World!");
/// }
/// ```
pub fn mock_client<MocksFn>(base_url: impl AsRef<str>, setup_mocks_fn: MocksFn) -> Client
where
    MocksFn: Fn(&mut Server<()>),
{
    let mut mocks_server = tide::new();
    setup_mocks_fn(&mut mocks_server);

    let mut mock_client = Client::with_http_client(mocks_server);
    mock_client.set_base_url(Url::parse(base_url.as_ref()).unwrap());

    mock_client
}

/// A test helper to assert on well structred errors produced by the `JsonErrorMiddleware`.
///
/// ```
/// use preroll::test_utils::{self, assert_json_error, TestResult};
///
/// #[async_std::main] // Would be #[async_std::test] instead.
/// async fn main() -> TestResult<()> {
///     let client = test_utils::create_client((), |_| {}).await.unwrap();
///
///     let mut res = client.get("/not_found").await.unwrap();
///
///     assert_json_error(
///         &mut res,
///         404,
///         "(no additional context)",
///     )
///     .await
///     .unwrap();
///     Ok(())
/// }
/// ```
#[allow(dead_code)] // Not actually dead code. (??)
pub async fn assert_json_error<Status>(
    mut res: impl AsMut<http::Response>,
    status: Status,
    err_msg: &str,
) -> TestResult<()>
where
    Status: TryInto<StatusCode>,
    Status::Error: Debug,
{
    let res = res.as_mut();

    let status: StatusCode = status
        .try_into()
        .expect("test must specify valid status code");

    let str_response = res.body_string().await?;

    let error: JsonError = serde_json::from_str(&str_response).map_err(|e| {
        surf::Error::from_str(
            res.status(),
            format!("Error, could not parse Response into JsonError! json err: \"{}\", response body: \"{}\"", e, str_response)
        )
    })?;

    assert_eq!(res.status(), status);
    assert_eq!(&error.title, status.canonical_reason());
    assert_eq!(error.message, err_msg);
    assert_eq!(error.status, status as u16);
    assert_eq!(
        error.request_id.as_str(),
        res["X-Request-Id"].last().as_str()
    );
    if res.status().is_server_error() {
        assert_eq!(
            error
                .correlation_id
                .expect("Internal server errors must have correlation ids.")
                .as_str(),
            res["X-Correlation-Id"].last().as_str()
        );
    } else {
        assert_eq!(error.correlation_id, None);
        assert!(res.header("X-Correlation-Id").is_none());
    }

    Ok(())
}

/// Assert that a response has a status code and parse out the body to JSON if possible.
///
/// This helper has better assertion failure messages than doing this manually.
///
/// ```
/// use preroll::test_utils::{self, assert_status_json, TestResult};
/// use preroll::JsonError;
///
/// #[async_std::main] // Would be #[async_std::test] instead.
/// async fn main() -> TestResult<()> {
///     let client = test_utils::create_client((), |_| {}).await.unwrap();
///
///     let mut res = client.get("/not_found").await.unwrap();
///
///     let json: JsonError = assert_status_json(&mut res, 404).await;
///     assert_eq!(&json.title, res.status().canonical_reason());
///
///     Ok(())
/// }
/// ```
pub async fn assert_status_json<StructType, Status>(
    mut res: impl AsMut<http::Response>,
    status: Status,
) -> StructType
where
    StructType: serde::de::DeserializeOwned,
    Status: TryInto<StatusCode>,
    Status::Error: Debug,
{
    let res = res.as_mut();

    let status: StatusCode = status
        .try_into()
        .expect("test must specify valid status code");

    let body = res.body_string().await.unwrap();

    assert_eq!(res.status(), status, "Response body: {}", body);

    serde_json::from_str(&body).unwrap_or_else(|err| {
        panic!(
            "Error: \"{}\" Body was not parseable into a {}, body was: \"{}\"",
            err,
            std::any::type_name::<StructType>(),
            body
        )
    })
}

/// Assert that a response has a specified status code.
///
/// This helper has better assertion failure messages than doing this manually.
///
/// ```
/// use preroll::test_utils::{self, assert_status, TestResult};
///
/// #[async_std::main] // Would be #[async_std::test] instead.
/// async fn main() -> TestResult<()> {
///     let client = test_utils::create_client((), |_| {}).await.unwrap();
///
///     let mut res = client.get("/monitor/ping").await.unwrap();
///
///     assert_status(&mut res, 200).await;
///     Ok(())
/// }
/// ```
pub async fn assert_status<Status>(mut res: impl AsMut<http::Response>, status: Status)
where
    Status: TryInto<StatusCode>,
    Status::Error: Debug,
{
    let res = res.as_mut();

    let status: StatusCode = status
        .try_into()
        .expect("test must specify valid status code");

    let body = res.body_string().await.unwrap();

    assert_eq!(res.status(), status, "Response body: {}", body);
}
