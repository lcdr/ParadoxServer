use std::{
    borrow::Borrow,
    convert::Infallible,
    error::Error,
    future::Ready,
    io,
    path::Path,
    str::{FromStr, Split, Utf8Error},
    sync::Arc,
    task::{self, Poll},
};

use assembly_core::buffer::CastError;
use assembly_fdb::mem::Database;
use assembly_xml::localization::LocaleNode;
use http::{
    header::{CONTENT_LENGTH, CONTENT_TYPE},
    HeaderValue, Request, Response,
};
use paradox_typed_db::TypedDatabase;
use percent_encoding::percent_decode_str;
use serde::Serialize;
use tower::Service;
use warp::{
    filters::BoxedFilter,
    reply::{Json, WithStatus},
    Filter, Reply,
};

use crate::data::locale::LocaleRoot;

use self::{
    docs::OpenApiService,
    files::make_crc_lookup_filter,
    rev::{make_api_rev, ReverseLookup},
};

pub mod adapter;
pub mod docs;
mod files;
mod locale;
pub mod rev;
pub mod tables;

#[derive(Clone, Debug)]
pub struct PercentDecoded(pub String);

impl FromStr for PercentDecoded {
    type Err = Utf8Error;
    #[inline]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = percent_decode_str(s).decode_utf8()?.to_string();
        Ok(PercentDecoded(s))
    }
}

impl Borrow<String> for PercentDecoded {
    fn borrow(&self) -> &String {
        &self.0
    }
}

impl ToString for PercentDecoded {
    #[inline]
    fn to_string(&self) -> String {
        self.0.clone()
    }
}

fn map_res<E: Error>(v: Result<Json, E>) -> WithStatus<Json> {
    match v {
        Ok(res) => wrap_200(res),
        Err(e) => wrap_500(warp::reply::json(&e.to_string())),
    }
}

fn map_opt_res<E: Error>(v: Result<Option<Json>, E>) -> WithStatus<Json> {
    match v {
        Ok(Some(res)) => wrap_200(res),
        Ok(None) => wrap_404(warp::reply::json(&())),
        Err(e) => wrap_500(warp::reply::json(&e.to_string())),
    }
}

fn map_opt(v: Option<Json>) -> WithStatus<Json> {
    match v {
        Some(res) => wrap_200(res),
        None => wrap_404(warp::reply::json(&())),
    }
}

fn wrap_404<A: Reply>(reply: A) -> WithStatus<A> {
    warp::reply::with_status(reply, warp::http::StatusCode::NOT_FOUND)
}

pub fn wrap_200<A: Reply>(reply: A) -> WithStatus<A> {
    warp::reply::with_status(reply, warp::http::StatusCode::OK)
}

pub fn wrap_500<A: Reply>(reply: A) -> WithStatus<A> {
    warp::reply::with_status(reply, warp::http::StatusCode::INTERNAL_SERVER_ERROR)
}

fn make_api_catch_all() -> impl Filter<Extract = (WithStatus<Json>,), Error = Infallible> + Clone {
    warp::any().map(|| warp::reply::json(&404)).map(wrap_404)
}

fn tydb_filter<'db>(
    db: &'db TypedDatabase<'db>,
) -> impl Filter<Extract = (&'db TypedDatabase<'db>,), Error = Infallible> + Clone + 'db {
    warp::any().map(move || db)
}

pub(crate) struct ApiFactory<'a> {
    pub tydb: &'static TypedDatabase<'static>,
    pub rev: &'static ReverseLookup,
    pub lr: Arc<LocaleNode>,
    pub res_path: &'a Path,
    pub pki_path: Option<&'a Path>,
}

impl<'a> ApiFactory<'a> {
    pub(crate) fn make_api(self) -> BoxedFilter<(WithStatus<Json>,)> {
        let loc = LocaleRoot::new(self.lr.clone());

        let v0_base = warp::path("v0");
        let v0_crc = warp::path("crc").and(make_crc_lookup_filter(self.res_path, self.pki_path));

        let v0_rev = warp::path("rev").and(make_api_rev(self.tydb, loc, self.rev));
        let v0 = v0_base.and(v0_crc.or(v0_rev).unify());

        // catch all
        let catch_all = make_api_catch_all();

        v0.or(catch_all).unify().boxed()
    }
}

enum ApiRoute<'r> {
    Tables,
    TableByName(&'r str),
    AllTableRows(&'r str),
    TableRowsByPK(&'r str, &'r str),
    Locale(Split<'r, char>),
    OpenApiV0,
}

impl<'r> ApiRoute<'r> {
    fn v0(mut parts: Split<'r, char>) -> Result<Self, ()> {
        match parts.next() {
            Some("tables") => match parts.next() {
                None => Ok(Self::Tables),
                Some(name) => match parts.next() {
                    None => Ok(Self::TableByName(name)),
                    Some("def") => match parts.next() {
                        None => Ok(Self::TableByName(name)),
                        _ => Err(()),
                    },
                    Some("all") => match parts.next() {
                        None => Ok(Self::AllTableRows(name)),
                        _ => Err(()),
                    },
                    Some(key) => match parts.next() {
                        None => Ok(Self::TableRowsByPK(name, key)),
                        _ => Err(()),
                    },
                },
            },
            Some("locale") => Ok(Self::Locale(parts)),
            Some("openapi.json") => match parts.next() {
                None => Ok(Self::OpenApiV0),
                _ => Err(()),
            },
            _ => Err(()),
        }
    }

    fn v1(mut parts: Split<'r, char>) -> Result<Self, ()> {
        match parts.next() {
            Some("tables") => match parts.next() {
                None => Ok(Self::Tables),
                _ => Err(()),
            },
            _ => Err(()),
        }
    }

    fn from_str(s: &'r str) -> Result<Self, ()> {
        let mut parts = s.trim_start_matches('/').split('/');
        match parts.next() {
            Some("v0") => Self::v0(parts),
            Some("v1") => Self::v1(parts),
            _ => Err(()),
        }
    }
}

#[derive(Clone)]
pub struct ApiService {
    pub db: Database<'static>,
    pub locale_root: Arc<LocaleNode>,
    pub openapi: OpenApiService,
}

fn into_other_io_error<E: std::error::Error + Send + Sync + 'static>(error: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, error)
}

#[allow(clippy::declare_interior_mutable_const)] // c.f. https://github.com/rust-lang/rust-clippy/issues/5812
const APPLICATION_JSON: HeaderValue = HeaderValue::from_static("application/json; charset=utf-8");

impl ApiService {
    pub fn new(
        db: Database<'static>,
        locale_root: Arc<LocaleNode>,
        openapi: OpenApiService,
    ) -> Self {
        Self {
            db,
            locale_root,
            openapi,
        }
    }

    fn reply_json_string(body: String) -> http::Response<hyper::Body> {
        let is_404 = body == "null";
        let content_length = HeaderValue::from(body.len());
        let mut r = Response::new(hyper::Body::from(body));

        if is_404 {
            // FIXME: hack; handle T = Option<U> for 404 properly
            *r.status_mut() = http::StatusCode::NOT_FOUND;
        }
        r.headers_mut().append(CONTENT_LENGTH, content_length);
        r.headers_mut().append(CONTENT_TYPE, APPLICATION_JSON);
        r
    }

    fn reply_json<T: Serialize>(v: &T) -> Result<http::Response<hyper::Body>, io::Error> {
        let body = serde_json::to_string(&v).map_err(into_other_io_error)?;
        Ok(Self::reply_json_string(body))
    }

    fn reply_404() -> http::Response<hyper::Body> {
        let mut r = Response::new(hyper::Body::from("404"));
        *r.status_mut() = http::StatusCode::NOT_FOUND;

        let content_length = HeaderValue::from(3);
        r.headers_mut().append(CONTENT_LENGTH, content_length);
        r.headers_mut().append(CONTENT_TYPE, APPLICATION_JSON);
        r
    }

    fn db_api<T: Serialize>(
        &self,
        f: impl FnOnce(Database<'static>) -> Result<T, CastError>,
    ) -> Result<Response<hyper::Body>, io::Error> {
        let v = f(self.db).map_err(into_other_io_error)?;
        Self::reply_json(&v)
    }

    /// Get data from `locale.xml`
    fn locale(&self, rest: Split<char>) -> Result<Response<hyper::Body>, io::Error> {
        match locale::select_node(self.locale_root.as_ref(), rest) {
            Some((node, locale::Mode::All)) => Self::reply_json(&locale::All::new(node)),
            Some((node, locale::Mode::Pod)) => Self::reply_json(&locale::Pod::new(node)),
            None => Ok(Self::reply_404()),
        }
    }
}

impl<ReqBody> Service<Request<ReqBody>> for ApiService {
    type Error = io::Error;
    type Response = Response<hyper::Body>;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    /// This is the main entry point to the API service.
    ///
    /// Here, we turn [ApiRoute]s into [http::Response]s
    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        let response = match ApiRoute::from_str(req.uri().path()) {
            Ok(ApiRoute::Tables) => self.db_api(tables::tables_json),
            Ok(ApiRoute::TableByName(name)) => self.db_api(|db| tables::table_def_json(db, name)),
            Ok(ApiRoute::AllTableRows(name)) => self.db_api(|db| tables::table_all_json(db, name)),
            Ok(ApiRoute::TableRowsByPK(name, key)) => {
                self.db_api(|db| tables::table_key_json(db, name, key))
            }
            Ok(ApiRoute::Locale(rest)) => self.locale(rest),
            Ok(ApiRoute::OpenApiV0) => Self::reply_json(self.openapi.as_ref()),
            Err(()) => Ok(Self::reply_404()),
        };
        std::future::ready(response)
    }
}
