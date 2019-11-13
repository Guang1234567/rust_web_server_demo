use std::collections::HashMap;
use std::env;
use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Error as IoError, ErrorKind as IoErrorKind};
use std::str::Utf8Error;
use std::string::FromUtf8Error;

use diesel::pg::PgConnection;
use diesel::prelude::*;
use futures::future::{err as futureErr, Future, FutureResult, ok as futureOk};
use futures::Stream;
use hyper::{Chunk, StatusCode};
use hyper::Error as hyperError;
use hyper::header::{ContentLength, ContentType};
use hyper::Method::{Get, Post};
use hyper::server::{Request, Response, Service};
use maud::html;
use url::form_urlencoded;

use dotenv::dotenv;

use super::data_source::models::Message;
use super::data_source::models::NewMessage;

const DEFAULT_DATABASE_URL: &'static str = "postgresql://postgres@localhost:5432";

pub struct MicroService;

impl Service for MicroService {
    type Request = Request;
    type Response = Response;
    type Error = hyper::Error;
    type Future = Box<dyn Future<Item=Self::Response, Error=Self::Error>>;

    fn call(&self, request: Request) -> Self::Future {
        let db_connection = match connect_to_db() {
            Some(connection) => connection,
            None => {
                return Box::new(futures::future::ok(
                    Response::new().with_status(StatusCode::InternalServerError),
                ));
            }
        };

        match (request.method(), request.path()) {
            (&Post, "/") => {
                let future = request
                    .body()
                    .concat2()
                    .and_then(parse_form)
                    .and_then(move |new_message| write_to_db(new_message, &db_connection))
                    .then(make_post_response);
                Box::new(future)
            }
            (&Get, "/") => {
                let time_range = match request.query() {
                    Some(query) => parse_query(query),
                    None => Ok(TimeRange {
                        before: None,
                        after: None,
                    }),
                };
                let response = match time_range {
                    Ok(time_range) => make_get_response(query_db(time_range, &db_connection)),
                    Err(error) => make_error_response(&error),
                };
                Box::new(response)
            }
            _ => Box::new(futureOk(Response::new().with_status(StatusCode::NotFound))),
        }
    }
}

/// https://juejin.im/post/5c7a3777f265da2dd773fc38
fn connect_to_db() -> Option<PgConnection> {
    // write .env to sysytem path
    dotenv().ok();

    let database_url = env::var("DATABASE_URL").unwrap_or(String::from(DEFAULT_DATABASE_URL));
    match PgConnection::establish(&database_url) {
        Ok(connection) => Some(connection),
        Err(error) => {
            error!("Error connection to database {}", error.description());
            None
        }
    }
}

fn query_db(time_range: TimeRange, db_connection: &PgConnection) -> Option<Vec<Message>> {
    use crate::schema::messages;
    let TimeRange { before, after } = time_range;
    let query_result = match (before, after) {
        (Some(before), Some(after)) => {
            messages::table
                .filter(messages::timestamp.lt(before as i64))
                .filter(messages::timestamp.gt(after as i64))
                .load::<Message>(db_connection)
        }
        (Some(before), _) => {
            messages::table
                .filter(messages::timestamp.lt(before as i64))
                .load::<Message>(db_connection)
        }
        (_, Some(after)) => {
            messages::table
                .filter(messages::timestamp.gt(after as i64))
                .load::<Message>(db_connection)
        }
        _ => {
            messages::table.load::<Message>(db_connection)
        }
    };

    match query_result {
        Ok(result) => Some(result),
        Err(error) => {
            error!("Error query Db: {}", error);
            None
        }
    }
}

fn write_to_db(new_message: NewMessage, db_connection: &PgConnection) -> FutureResult<i64, hyper::Error> {
    use crate::schema::messages;
    let timestamp = diesel::insert_into(messages::table)
        .values(&new_message)
        .returning(messages::timestamp)
        .get_result(db_connection);
    match timestamp {
        Ok(timestamp) => futures::future::ok(timestamp),
        Err(error) => {
            error!("Error writing to database: {}", error.description());
            futures::future::err(hyper::Error::from(IoError::new(IoErrorKind::Other, "service error")))
        }
    }
}

fn parse_form(form_chunk: Chunk) -> FutureResult<NewMessage, hyperError> {
    let mut form = form_urlencoded::parse(form_chunk.as_ref())
        .into_owned()
        .collect::<HashMap<String, String>>();
    if let Some(message) = form.remove("message") {
        let username = form.remove("username").unwrap_or(String::from("anonymous"));
        futureOk(NewMessage {
            username,
            message,
        })
    } else {
        futureErr(hyperError::from(IoError::new(
            IoErrorKind::InvalidInput,
            "Missing field message",
        )))
    }
}

fn make_post_response(result: Result<i64, hyperError>) -> FutureResult<hyper::Response, hyperError> {
    match result {
        Ok(timestamp) => {
            let payload = json!({"timestamp": timestamp}).to_string();
            let response = Response::new()
                .with_header(ContentLength(payload.len() as u64))
                .with_header(ContentType::json())
                .with_body(payload);
            debug!("{:?}", response);
            futureOk(response)
        }
        Err(error) => make_error_response(error.description()),
    }
}

fn make_error_response(error_message: &str) -> FutureResult<hyper::Response, hyper::Error> {
    let payload = json!({"error": error_message}).to_string();
    let response = Response::new()
        .with_status(StatusCode::InternalServerError)
        .with_header(ContentLength(payload.len() as u64))
        .with_header(ContentType::json())
        .with_body(payload);
    debug!("{:?}", response);
    futures::future::ok(response)
}


struct TimeRange {
    before: Option<i64>,
    after: Option<i64>,
}

fn parse_query(query: &str) -> Result<TimeRange, String> {
    let args = form_urlencoded::parse(&query.as_bytes())
        .into_owned()
        .collect::<HashMap<String, String>>();
    let before = args.get("before").map(|value| value.parse::<i64>());
    if let Some(ref result) = before {
        if let Err(ref error) = *result {
            return Err(format!("Error parsing 'before: {}", error));
        }
    }

    let after = args.get("after").map(|value| value.parse::<i64>());
    if let Some(ref result) = after {
        if let Err(ref error) = *result {
            return Err(format!("Error parsing 'after': {}", error));
        }
    }
    Ok(TimeRange {
        before: before.map(|b| b.unwrap()),
        after: after.map(|b| b.unwrap()),
    })
}

fn make_get_response(messages: Option<Vec<Message>>) -> FutureResult<hyper::Response, hyper::Error> {
    let response = match messages {
        Some(messages) => {
            let body = render_html(messages);
            Response::new()
                .with_header(ContentLength(body.len() as u64))
                .with_body(body)
        }
        None => Response::new().with_status(StatusCode::InternalServerError),
    };
    debug!("{:?}", response);
    futures::future::ok(response)
}

/// https://maud.lambda.xyz/partials.html
fn render_html(messages: Vec<Message>) -> String {
    (html! {
        head {
            title { "microservice" }
            //style {"body { font-family: monospace }"}
            meta charset="utf-8";
        }
        body {
            ul {
                @for message in &messages {
                    li {
                        (message.username) " (" (message.timestamp) "): " (message.message)
                    }
                }
            }
        }
    }).into_string()
}