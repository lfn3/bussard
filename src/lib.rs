use http::header::{HeaderMap, HeaderName, HeaderValue, InvalidHeaderName, InvalidHeaderValue};
use http::{response::Builder, Method};
use hyper::{self, body::Bytes};
use pyo3::prelude::{pyclass, pymethods, PyObject, PyResult, Python};
use pyo3::PyAny;
use pyo3::{
    types::{IntoPyDict, PyDict, PyString, PyTuple},
    PyCell, IntoPy,
};
use std::{cell::RefCell, collections::HashMap, env, fmt::Debug, rc::Rc, cmp::max};
use tokio::sync::{mpsc, oneshot};
use warp::{path::Tail, Rejection};

fn add_per_request_environ<'py>(py: Python<'py>, req: BussardRequest) -> PyResult<&'py PyDict> {
    let str_env_vars = vec![
        ("wsgi.url_scheme", "http"),
        ("HTTP_HOST", "localhost"),
        ("REQUEST_METHOD", req.method.as_str()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()));

    let with_external_env = env::vars().into_iter().chain(str_env_vars);

    let py_env = with_external_env
        .map(|(k, v)| (k, PyString::new(py, v.as_str())))
        .into_py_dict(py);

    if !req.path.is_empty() {
        py_env.set_item("PATH_INFO", req.path.as_str())?;
    }
    py_env.set_item("CONTENT_LENGTH", req.body.len())?;

    let py_req: PyObject = req.into_py(py);
    py_env.set_item("wsgi.input", py_req)?;
    py_env.set_item("wsgi.version", PyTuple::new(py, vec![1, 0]))?;
    py_env.set_item("wsgi.run_once", false)?;

    Ok(py_env)
}

#[pyclass]
#[derive(Clone)]
struct StartResponse {
    response_prelude: Rc<RefCell<Option<ResponsePrelude>>>
}

#[derive(Debug)]
struct ResponsePrelude {
    status_code: hyper::http::StatusCode,
    headers: hyper::http::HeaderMap
}

#[pymethods]
impl StartResponse {
    #[call]
    fn __call__(&self, py: Python, status: &str, response_headers: Vec<(&str, &str)>, _exc_info: Option<&PyTuple>) {
        // TODO: handle exc_info, replacing headers.
        if self.has_already_been_set_or_cannot_borrow() {
            return; //TODO: raise exception instead?
        }

        let response_prelude = py.allow_threads(move || {
            // TODO: error handling
            let headers = normalize_headers(response_headers).unwrap();
            let status_code: hyper::http::StatusCode = status.split(' ').nth(0)
                                                                .and_then(|s| str::parse::<u16>(s).ok())
                                                                .and_then(|u| hyper::http::StatusCode::from_u16(u).ok()).unwrap();
            ResponsePrelude{headers, status_code}
        });

        self.response_prelude.replace(Some(response_prelude));
    }
}

impl StartResponse {
    fn new() -> StartResponse {
        StartResponse {
            response_prelude: Rc::new(RefCell::new(Option::None)),
        }
    }

    fn has_already_been_set_or_cannot_borrow(&self) -> bool {
        self.response_prelude.try_borrow().map(|r| r.is_some()).unwrap_or(false)
    }
}

#[pyclass]
#[derive(Debug)]
pub struct BussardRequest {
    path: String,
    header_map: HeaderMap,
    method: Method,
    body: Bytes,
    _index: usize
}

#[pymethods]
impl BussardRequest {
    fn read(&mut self, size: Option<usize>) -> &[u8] {
        // TODO: test this - I don't trust it.
        
        let start = self._index;
        let remaining = self.body.len() - start;
        let to_get = max(remaining, size.unwrap_or(0));
        
        if to_get == 0 {
            return &[];
        }

        let result = self.body.get(start..start + to_get).unwrap();

        self._index += to_get;

        result
    }

    fn readline(&mut self, size: Option<usize>) -> &[u8] {
        self.read(size) //TODO: actually split on newlines
    }
}

impl BussardRequest {
    fn new(path: String, header_map: HeaderMap, method: Method, body: Bytes) -> BussardRequest {
        BussardRequest {
            path,
            header_map,
            method,
            body,
            _index: 0
        }
    }
}

#[derive(Debug)]
pub struct AsyncBussardRequest {
    req: BussardRequest,
    body_sender: hyper::body::Sender,
    response_prelude_sender: oneshot::Sender<ResponsePrelude>,
}

impl AsyncBussardRequest {
    fn new(
        req: BussardRequest,
        body_sender: hyper::body::Sender,
        response_prelude_sender: oneshot::Sender<ResponsePrelude>,
    ) -> AsyncBussardRequest {
        AsyncBussardRequest {
            req,
            body_sender,
            response_prelude_sender,
        }
    }
}

fn invoke_app_py<'a>(
    py: Python,
    app: &'a PyAny,
    req: BussardRequest,
    sr: StartResponse,
) -> PyResult<&'a PyAny> {
    let full_environ: &PyAny = add_per_request_environ(py, req)?.into();
    let py_sr = PyCell::new(py, sr)?;
    let args = PyTuple::new(py, vec![full_environ, py_sr]);
    app.call1(args)
}

fn invoke_app<'a>(
    py: Python,
    app: &'a PyAny,
    req: BussardRequest,
) -> PyResult<(&'a PyAny, ResponsePrelude)> {
    let sr = StartResponse::new();
    let result = invoke_app_py(py, app, req, sr.clone())?;
    Ok((result, sr.response_prelude.replace(None).unwrap()))
}

async fn send_resp<'body_sender>(
    body_sender: &mut hyper::body::Sender,
    response_prelude_sender: oneshot::Sender<ResponsePrelude>,
    resp: PyResult<(&PyAny, ResponsePrelude)>,
) -> PyResult<()> {
    let (body, headers) = resp?;
    let bytes_iter = body.iter()?.map(|b| b.and_then(PyAny::extract::<Vec<u8>>));

    response_prelude_sender.send(headers).unwrap();
    for byte_vec in bytes_iter {
        let bytes = Bytes::from(byte_vec?);
        body_sender.send_data(bytes).await.unwrap()
    }

    Ok(())
}

pub async fn bussard<'body, C>(receiver: &mut mpsc::Receiver<AsyncBussardRequest>, app_ctor: C)
where
    C: FnOnce(Python) -> PyResult<&PyAny>,
{
    let gil = Python::acquire_gil();
    let py = gil.python();
    let app = app_ctor(py).unwrap();

    loop {
        match receiver.recv().await {
            Some(mut req) => {
                let resp = invoke_app(py, app, req.req);
                send_resp(&mut req.body_sender, req.response_prelude_sender, resp)
                    .await
                    .unwrap();
            }
            None => {
                break;
            }
        }
    }
}

fn normalize_headers(headers: Vec<(&str, &str)>) -> Result<HeaderMap, Rejection> {
    let normalized_names: Result<Vec<(HeaderName, &str)>, InvalidHeaderName> = headers
        .into_iter()
        .map(|(k, v)| HeaderName::from_bytes(k.as_bytes()).map(|hn| (hn, v)))
        .collect();
    let normalized_values: Result<HashMap<HeaderName, HeaderValue>, InvalidHeaderValue> =
        normalized_names
            .unwrap()
            .into_iter()
            .map(|(hn, v)| HeaderValue::from_bytes(v.as_bytes()).map(|hv| (hn, hv)))
            .collect();
    let unwrapped = normalized_values.unwrap();

    let mut hm = HeaderMap::with_capacity(unwrapped.len());
    hm.extend(unwrapped);
    Ok(hm)
}

pub async fn dispatch_req(
    path: Tail,
    header_map: HeaderMap,
    method: Method,
    body: Bytes,
    mut sender: mpsc::Sender<AsyncBussardRequest>,
) -> Result<http::Response<hyper::Body>, Rejection> {
    let path = path.as_str().to_owned();
    let req = BussardRequest::new(path, header_map, method, body);
    let (body_sender, body) = hyper::Body::channel();

    let (response_prelude_sender, response_prelude_reciever) = oneshot::channel::<ResponsePrelude>();

    let aync_req = AsyncBussardRequest::new(req, body_sender, response_prelude_sender);
    sender.send(aync_req).await.unwrap();

    let response_prelude = response_prelude_reciever.await.unwrap();

    let mut builder = Builder::new().status(response_prelude.status_code);
    builder.headers_mut().unwrap().extend(response_prelude.headers.into_iter());
    Ok(builder.body(body).unwrap())
}

#[cfg(test)]
mod tests {

    use crate::{invoke_app, StartResponse, ResponsePrelude, BussardRequest};
    use http::{HeaderMap, Method};
    use hyper::body::Bytes;
    use pyo3::{types::PyList, PyAny, PyResult, PyTryInto, Python};
    use std::{env, str::from_utf8};

    fn add_paths(py: Python) -> PyResult<()> {
        let syspath: &PyList = py.import("sys")?.get("path")?.try_into()?;

        let mut cwd = env::current_dir().unwrap();
        cwd.push("src");
        syspath.insert(0, cwd.to_str())?;

        cwd.pop();
        cwd.push(".venv");
        cwd.push("Lib");
        cwd.push("site-packages");
        syspath.insert(1, cwd.to_str())?;

        Ok(())
    }

    fn flaskapp(py: Python) -> PyResult<&PyAny> {
        add_paths(py)?;

        let flask_main = py.import("main")?;
        let flask_app = flask_main.get("make_app")?.call0()?;

        Ok(flask_app)
    }

    #[test]
    fn test_sync_wsgi() -> PyResult<()> {
        let gil = Python::acquire_gil();
        let py = gil.python();
        let app = flaskapp(py)?;

        let req = BussardRequest::new("".to_owned(), HeaderMap::new(), Method::GET, Bytes::from_static(&[]));
        let (res, response_prelude) = invoke_app(py, app, req)?;
        assert_eq!(response_prelude.headers.get("Content-Length").unwrap(), "13");

        let body: Vec<Vec<u8>> = res
            .iter()?
            .collect::<PyResult<Vec<&PyAny>>>()?
            .into_iter()
            .map(PyAny::extract::<Vec<u8>>)
            .collect::<PyResult<Vec<Vec<u8>>>>()?;

        assert_eq!(body.len(), 1);

        let first_as_str = from_utf8(body[0].as_slice()).unwrap();
        assert_eq!(first_as_str, "Hello, World!");

        Ok(())
    }

    #[test]
    fn test_post() -> PyResult<()> {
        let gil = Python::acquire_gil();
        let py = gil.python();
        let app = flaskapp(py)?;

        let req = BussardRequest::new("/echo".to_owned(), HeaderMap::new(), Method::POST, Bytes::from_static("ping".as_bytes()));
        let (res, response_prelude) = invoke_app(py, app, req)?;
        assert_eq!(response_prelude.headers.get("Content-Length").unwrap(), "4");

        let body: Vec<Vec<u8>> = res
            .iter()?
            .collect::<PyResult<Vec<&PyAny>>>()?
            .into_iter()
            .map(PyAny::extract::<Vec<u8>>)
            .collect::<PyResult<Vec<Vec<u8>>>>()?;

        assert_eq!(body.len(), 1);

        let first_as_str = from_utf8(body[0].as_slice()).unwrap();
        assert_eq!(first_as_str, "ping");

        Ok(())
    }
    
    #[test]
    fn test_404() -> PyResult<()> {
        let gil = Python::acquire_gil();
        let py = gil.python();
        let app = flaskapp(py)?;

        let req = BussardRequest::new("/404".to_owned(), HeaderMap::new(), Method::POST, Bytes::from_static(&[]));
        let (res, response_prelude) = invoke_app(py, app, req)?;
        assert_eq!(response_prelude.headers.get("Content-Length").unwrap(), "232");
        assert_eq!(response_prelude.status_code,  hyper::http::StatusCode::NOT_FOUND);

        let body: Vec<Vec<u8>> = res
            .iter()?
            .collect::<PyResult<Vec<&PyAny>>>()?
            .into_iter()
            .map(PyAny::extract::<Vec<u8>>)
            .collect::<PyResult<Vec<Vec<u8>>>>()?;

        assert_eq!(body.len(), 1);

        let first_as_str = from_utf8(body[0].as_slice()).unwrap();
        assert!(first_as_str.contains("<title>404 Not Found</title>"));

        Ok(())
    }

    
    #[test]
    fn test_error() -> PyResult<()> {
        let gil = Python::acquire_gil();
        let py = gil.python();
        let app = flaskapp(py)?;

        let req = BussardRequest::new("/throw".to_owned(), HeaderMap::new(), Method::GET, Bytes::from_static(&[]));
        let (res, response_prelude) = invoke_app(py, app, req)?;
        assert_eq!(response_prelude.headers.get("Content-Length").unwrap(), "290");
        assert_eq!(response_prelude.status_code,  hyper::http::StatusCode::INTERNAL_SERVER_ERROR);

        let body: Vec<Vec<u8>> = res
            .iter()?
            .collect::<PyResult<Vec<&PyAny>>>()?
            .into_iter()
            .map(PyAny::extract::<Vec<u8>>)
            .collect::<PyResult<Vec<Vec<u8>>>>()?;

        assert_eq!(body.len(), 1);

        let first_as_str = from_utf8(body[0].as_slice()).unwrap();
        assert!(first_as_str.contains("<title>500 Internal Server Error</title>"));

        Ok(())
    }

    #[test]
    fn test_has_already_been_set_or_cannot_borrow_for_unused_start_response() {
        assert!(!StartResponse::new().has_already_been_set_or_cannot_borrow())
    }

    #[test]
    fn test_has_already_been_set_or_cannot_borrow_for_used_start_response() {
        let sr = StartResponse::new();
        let prelude = ResponsePrelude{headers: HeaderMap::new(), status_code: hyper::http::StatusCode::OK};
        sr.response_prelude.replace(Some(prelude));
        assert!(sr.has_already_been_set_or_cannot_borrow())
    }
}
