use core::str::Utf8Error;
use std::fs;
use std::io;
use std::path::*;
use std::ops::Deref;
use std::sync::Arc;

use iron::prelude::*;
use iron::headers::CookiePair;
use iron::{BeforeMiddleware, status};
use mount::Mount;
use oven::prelude::*;
use params;
use rustc_serialize::json;
use url::percent_encoding::percent_decode;

use collection::*;
use error::*;
use thumbnails::*;
use utils::*;

const CURRENT_MAJOR_VERSION: i32 = 1;
const CURRENT_MINOR_VERSION: i32 = 0;

#[derive(RustcEncodable)]
struct Version {
    major: i32,
    minor: i32,
}

impl Version {
    fn new(major: i32, minor: i32) -> Version {
        Version {
            major: major,
            minor: minor,
        }
    }
}

impl From<PError> for IronError {
    fn from(err: PError) -> IronError {
        match err {
            PError::Io(e) => IronError::new(e, status::NotFound),
            PError::CannotClearExistingIndex => IronError::new(err, status::InternalServerError),
            PError::PathDecoding => IronError::new(err, status::InternalServerError),
            PError::ConfigDirectoryError => IronError::new(err, status::InternalServerError),
            PError::CacheDirectoryError => IronError::new(err, status::InternalServerError),
            PError::PathNotInVfs => IronError::new(err, status::NotFound),
            PError::CannotServeDirectory => IronError::new(err, status::BadRequest),
            PError::UnsupportedFileType => IronError::new(err, status::BadRequest),
            PError::AlbumArtSearchError => IronError::new(err, status::InternalServerError),
            PError::ImageProcessingError => IronError::new(err, status::InternalServerError),
            PError::UnsupportedMetadataFormat => IronError::new(err, status::InternalServerError),
            PError::MetadataDecodingError => IronError::new(err, status::InternalServerError),
            PError::Unauthorized => IronError::new(err, status::Unauthorized),
            PError::IncorrectCredentials => IronError::new(err, status::BadRequest),
        }
    }
}

pub fn get_api_handler(collection: Arc<Collection>) -> Mount {
    let mut api_handler = Mount::new();

    {
        let collection = collection.clone();
        api_handler.mount("/version/", self::version);
        api_handler.mount("/auth/",
                          move |request: &mut Request| self::auth(request, collection.deref()));
    }

    {
        let mut auth_api_mount = Mount::new();
        {
            let collection = collection.clone();
            auth_api_mount.mount("/browse/", move |request: &mut Request| {
                self::browse(request, collection.deref())
            });
        }
        {
            let collection = collection.clone();
            auth_api_mount.mount("/flatten/", move |request: &mut Request| {
                self::flatten(request, collection.deref())
            });
        }
        {
            let collection = collection.clone();
            auth_api_mount.mount("/serve/", move |request: &mut Request| {
                self::serve(request, collection.deref())
            });
        }

        let mut auth_api_chain = Chain::new(auth_api_mount);
        auth_api_chain.link_before(AuthRequirement);

        api_handler.mount("/", auth_api_chain);
    }
    api_handler
}

fn path_from_request(request: &Request) -> Result<PathBuf, Utf8Error> {
    let path_string = request.url.path().join("\\");
    let decoded_path = try!(percent_decode(path_string.as_bytes()).decode_utf8());
    Ok(PathBuf::from(decoded_path.deref()))
}

struct AuthRequirement;
impl BeforeMiddleware for AuthRequirement {
    fn before(&self, req: &mut Request) -> IronResult<()> {
        let auth_cookie = req.get_cookie("username");
        if auth_cookie.is_some() {
            Ok(())
        } else {
            Err(IronError::new(PError::Unauthorized, status::Unauthorized))
        }
    }
}

fn version(_: &mut Request) -> IronResult<Response> {
    let current_version = Version::new(CURRENT_MAJOR_VERSION, CURRENT_MINOR_VERSION);
    match json::encode(&current_version) {
        Ok(result_json) => Ok(Response::with((status::Ok, result_json))),
        Err(e) => Err(IronError::new(e, status::InternalServerError)),
    }
}

fn auth(request: &mut Request, collection: &Collection) -> IronResult<Response> {
    let input = request.get_ref::<params::Params>().unwrap();
    let username = match input.find(&["username"]) {
        Some(&params::Value::String(ref username)) => username,
        _ => return Err(IronError::from(PError::IncorrectCredentials)),
    };
    let password = match input.find(&["password"]) {
        Some(&params::Value::String(ref password)) => password,
        _ => return Err(IronError::from(PError::IncorrectCredentials)),
    };
    if collection.auth(username.as_str(), password.as_str()) {
        let mut response = Response::with((status::Ok, ""));
        let mut username_cookie = CookiePair::new("username".to_string(), username.clone());
        username_cookie.path = Some("/".to_owned());
        response.set_cookie(username_cookie);
        Ok(response)
    } else {
        Err(IronError::from(PError::IncorrectCredentials))
    }
}

fn browse(request: &mut Request, collection: &Collection) -> IronResult<Response> {
    let path = path_from_request(request);
    let path = match path {
        Err(e) => return Err(IronError::new(e, status::BadRequest)),
        Ok(p) => p,
    };
    let browse_result = try!(collection.browse(&path));

    let result_json = json::encode(&browse_result);
    let result_json = match result_json {
        Ok(j) => j,
        Err(e) => return Err(IronError::new(e, status::InternalServerError)),
    };

    Ok(Response::with((status::Ok, result_json)))
}

fn flatten(request: &mut Request, collection: &Collection) -> IronResult<Response> {
    let path = path_from_request(request);
    let path = match path {
        Err(e) => return Err(IronError::new(e, status::BadRequest)),
        Ok(p) => p,
    };
    let flatten_result = try!(collection.flatten(&path));

    let result_json = json::encode(&flatten_result);
    let result_json = match result_json {
        Ok(j) => j,
        Err(e) => return Err(IronError::new(e, status::InternalServerError)),
    };

    Ok(Response::with((status::Ok, result_json)))
}

fn serve(request: &mut Request, collection: &Collection) -> IronResult<Response> {
    let virtual_path = path_from_request(request);
    let virtual_path = match virtual_path {
        Err(e) => return Err(IronError::new(e, status::BadRequest)),
        Ok(p) => p,
    };

    let real_path = collection.locate(virtual_path.as_path());
    let real_path = match real_path {
        Err(e) => return Err(IronError::new(e, status::NotFound)),
        Ok(p) => p,
    };

    let metadata = match fs::metadata(real_path.as_path()) {
        Ok(meta) => meta,
        Err(e) => {
            let status = match e.kind() {
                io::ErrorKind::NotFound => status::NotFound,
                io::ErrorKind::PermissionDenied => status::Forbidden,
                _ => status::InternalServerError,
            };
            return Err(IronError::new(e, status));
        }
    };

    if !metadata.is_file() {
        return Err(IronError::from(PError::CannotServeDirectory));
    }

    if is_song(real_path.as_path()) {
        return Ok(Response::with((status::Ok, real_path)));
    }

    if is_image(real_path.as_path()) {
        return art(request, real_path.as_path());
    }

    Err(IronError::from(PError::UnsupportedFileType))
}

fn art(_: &mut Request, real_path: &Path) -> IronResult<Response> {
    let thumb = get_thumbnail(real_path, 400);
    match thumb {
        Ok(path) => Ok(Response::with((status::Ok, path))),
        Err(e) => Err(IronError::from(e)),
    }
}
