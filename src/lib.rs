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
    headers: Rc<RefCell<Option<HashMap<String, String>>>>,
}

#[pymethods]
impl StartResponse {
    #[call]
    #[args(args = "*")]
    fn __call__(&mut self, args: &PyTuple) {
        // TODO: This function is _super_ unsafe
        let _status_str: &str = args.get_item(0).extract().unwrap(); // TODO: use this
        let headers_list: Vec<(&str, &str)> = args.get_item(1).extract().unwrap();

        let header_map = headers_list
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        self.headers.replace(Some(header_map));
    }
}

impl StartResponse {
    fn new() -> StartResponse {
        StartResponse {
            headers: Rc::new(RefCell::new(Option::None)),
        }
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

#[derive(Debug)]
pub struct AsyncBussardRequest {
    req: BussardRequest,
    body_sender: hyper::body::Sender,
    headers_sender: oneshot::Sender<HashMap<String, String>>,
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
) -> PyResult<(&'a PyAny, HashMap<String, String>)> {
    let sr = StartResponse::new();
    let result = invoke_app_py(py, app, req, sr.clone())?;
    Ok((result, sr.headers.replace(None).unwrap()))
}

async fn send_resp<'body_sender>(
    body_sender: &mut hyper::body::Sender,
    headers_sender: oneshot::Sender<HashMap<String, String>>,
    resp: PyResult<(&PyAny, HashMap<String, String>)>,
) -> PyResult<()> {
    let (body, headers) = resp?;
    let bytes_iter = body.iter()?.map(|b| b.and_then(PyAny::extract::<Vec<u8>>));

    headers_sender.send(headers).unwrap();
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
                send_resp(&mut req.body_sender, req.headers_sender, resp)
                    .await
                    .unwrap();
            }
            None => {
                break;
            }
        }
    }
}

fn build_req(path: String, header_map: HeaderMap, method: Method, body: Bytes) -> BussardRequest {
    BussardRequest {
        path,
        header_map,
        method,
        body,
        _index: 0
    }
}

fn build_async_req(
    req: BussardRequest,
    resp_sender: hyper::body::Sender,
    headers_sender: oneshot::Sender<HashMap<String, String>>,
) -> AsyncBussardRequest {
    AsyncBussardRequest {
        req,
        body_sender: resp_sender,
        headers_sender,
    }
}

fn normalize_headers(headers: HashMap<String, String>) -> Result<HeaderMap, Rejection> {
    let normalized_names: Result<Vec<(HeaderName, String)>, InvalidHeaderName> = headers
        .into_iter()
        .map(|(k, v)| HeaderName::from_bytes(k.into_bytes().as_slice()).map(|hn| (hn, v)))
        .collect();
    let normalized_values: Result<HashMap<HeaderName, HeaderValue>, InvalidHeaderValue> =
        normalized_names
            .unwrap()
            .into_iter()
            .map(|(hn, v)| HeaderValue::from_bytes(v.into_bytes().as_slice()).map(|hv| (hn, hv)))
            .collect();
    let unwrapped = normalized_values.unwrap();

    let mut hm = HeaderMap::with_capacity(unwrapped.len());
    hm.extend(unwrapped);
    Ok(hm)
}

pub async fn dispatch_req<'body>(
    path: Tail,
    header_map: HeaderMap,
    method: Method,
    body: Bytes,
    mut sender: mpsc::Sender<AsyncBussardRequest>,
) -> Result<http::Response<hyper::Body>, Rejection> {
    let path = path.as_str().to_owned();
    let req = build_req(path, header_map, method, body);
    let (body_sender, body) = hyper::Body::channel();

    let (headers_sender, headers_reciever) = oneshot::channel::<HashMap<String, String>>();

    let aync_req = build_async_req(req, body_sender, headers_sender);
    sender.send(aync_req).await.unwrap();

    let headers = normalize_headers(headers_reciever.await.unwrap())?;

    let mut builder = Builder::new();
    builder.headers_mut().unwrap().extend(headers.into_iter());

    Ok(builder.body(body).unwrap())
}

#[cfg(test)]
mod tests {

    use crate::{invoke_app, build_req};
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

        let req = build_req("".to_owned(), HeaderMap::new(), Method::GET, Bytes::from_static(&[]));
        let (res, headers) = invoke_app(py, app, req)?;
        assert_eq!(headers.get("Content-Length").unwrap(), "13");

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
        let app = flaskapp(py).map_err(|e| { e.print_and_set_sys_last_vars(py) }).unwrap();

        let req = build_req("/echo".to_owned(), HeaderMap::new(), Method::POST, Bytes::from_static("ping".as_bytes()));
        let (res, headers) = invoke_app(py, app, req)?;
        assert_eq!(headers.get("Content-Length").unwrap(), "4");

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
        let app = flaskapp(py).map_err(|e| { e.print_and_set_sys_last_vars(py) }).unwrap();

        let req = build_req("/404".to_owned(), HeaderMap::new(), Method::POST, Bytes::from_static(&[]));
        let (res, headers) = invoke_app(py, app, req)?;
        assert_eq!(headers.get("Content-Length").unwrap(), "232");

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
}
