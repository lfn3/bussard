use http::header::{HeaderMap, HeaderName, HeaderValue, InvalidHeaderName, InvalidHeaderValue};
use http::{response::Builder, Method};
use hyper::{self, body::Bytes, };
use pyo3::prelude::{pyclass, pymethods, PyObject, PyResult, Python};
use pyo3::{
    types::{IntoPyDict, PyDict, PyList, PyString, PyTuple},
    PyCell,
};
use pyo3::{PyAny, PyTryInto};
use std::{cell::RefCell, collections::HashMap, env, rc::Rc};
use tokio::sync::{mpsc, oneshot};
use tokio::{join, task};
use warp::{filters::header::headers_cloned, Filter, Rejection};

fn add_per_request_environ(py: Python, req: BussardRequest) -> PyResult<&PyDict> {
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
    fn __call__(&mut self, py: Python, args: &PyTuple) {
        // TODO: This function is _super_ unsafe
        let status_str: &str = args.get_item(0).extract().unwrap();
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

#[derive(Debug)]
struct BussardRequest {
    header_map: HeaderMap,
    method: Method,
}

#[derive(Debug)]
struct AsyncBussardRequest {
    req: BussardRequest,
    resp_sender: hyper::body::Sender,
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
) -> (PyResult<&'a PyAny>, HashMap<String, String>) {
    let sr = StartResponse::new();
    let result = invoke_app_py(py, app, req, sr.clone());
    (result, sr.headers.replace(None).unwrap())
}

async fn bussard(receiver: &mut mpsc::Receiver<AsyncBussardRequest>) {
    let gil = Python::acquire_gil();
    let py = gil.python();
    let app = flaskapp(py).unwrap();

    loop {
        match receiver.recv().await {
            Some(mut req) => {
                let (resp, headers) = invoke_app(py, app, req.req);
                match resp {
                    Ok(resp) => {
                        req.headers_sender.send(headers).unwrap();

                        req.resp_sender
                            .send_data(Bytes::from(format!("Hello. Full body is: <pre><code>{}</code></pre>", resp)))
                            .await
                            .unwrap();
                    }
                    Err(e) => {
                        e.print_and_set_sys_last_vars(py);
                    }
                }
            }
            None => {
                break;
            }
        }
    }
}

fn with<T: Sized + Clone + Send>(
    t: T,
) -> impl Filter<Extract = (T,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || t.clone())
}

fn build_req(header_map: HeaderMap, method: Method) -> BussardRequest {
    BussardRequest { header_map, method }
}

fn build_async_req(
    req: BussardRequest,
    resp_sender: hyper::body::Sender,
    headers_sender: oneshot::Sender<HashMap<String, String>>,
) -> AsyncBussardRequest {
    AsyncBussardRequest {
        req,
        resp_sender,
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

async fn dispatch_req(
    header_map: HeaderMap,
    method: Method,
    mut sender: mpsc::Sender<AsyncBussardRequest>,
) -> Result<http::Response<hyper::Body>, Rejection> {
    let req = build_req(header_map, method);
    let (body_sender, resp_body) = hyper::Body::channel();

    let headers_ch = oneshot::channel::<HashMap<String, String>>();
    let headers_sender = headers_ch.0;
    let headers_reciever = headers_ch.1;

    let aync_req = build_async_req(req, body_sender, headers_sender);
    sender.send(aync_req).await.unwrap();

    let headers = normalize_headers(headers_reciever.await.unwrap())?;

    let mut builder = Builder::new();
    builder.headers_mut().unwrap().extend(headers.into_iter());

    Ok(builder.body(resp_body).unwrap())
}

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

#[tokio::main]
async fn main() {
    let mut ch = mpsc::channel::<AsyncBussardRequest>(1024);
    let receiver: &mut mpsc::Receiver<AsyncBussardRequest> = &mut ch.1;
    // We block in place so we don't have to send the python bits
    let bussard = task::block_in_place(move || bussard(receiver));

    let bussarded = warp::any()
        .and(headers_cloned())
        .and(warp::method())
        .and(with(ch.0))
        .and_then(dispatch_req);

    let server = warp::serve(bussarded).run(([127, 0, 0, 1], 3030));
    //.then(|x| { ch.1.close(); ready(()) }); // TODO: this, but we've already handed off the reciever

    join!(bussard, server);
}

#[cfg(test)]
mod tests {

    use crate::{flaskapp, invoke_app, BussardRequest};
    use http::{HeaderMap, Method};
    use pyo3::{PyResult, Python};

    #[test]
    fn test_sync_wsgi() -> PyResult<()> {
        let gil = Python::acquire_gil();
        let py = gil.python();
        let app = flaskapp(py)?;

        let req = BussardRequest {
            header_map: HeaderMap::new(),
            method: Method::GET,
        };
        let (res, headers) = invoke_app(py, app, req);
        res?;
        println!("headers: {:?}", headers);

        Ok(())
    }
}
