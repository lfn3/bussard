use futures::Future;
use http::header::HeaderMap;
use http::Method;
use pyo3::prelude::{pyclass, pymethods, PyObject, PyResult, Python};
use pyo3::{
    types::{IntoPyDict, PyDict, PyList, PyString, PyTuple},
    PyCell,
};
use pyo3::{PyAny, PyTryInto};
use std::{collections::HashMap, env, task::Poll};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::{join, task};
use warp::{filters::header::headers_cloned, Filter, Rejection};

fn add_per_request_environ(
    py: Python,
    req: BussardRequest,
) -> &PyDict {
    let environ = base_environ();
    let py_env = environ
        .iter()
        .map(|(k, v)| (k, PyString::new(py, v)))
        .into_py_dict(py);

    py_env
        .set_item("REQUEST_METHOD", req.method.as_str())
        .unwrap();

    py_env
}

#[pyclass]
struct StartResponse {
    headers: Box<Option<HashMap<String, String>>>,
}

#[pymethods]
impl StartResponse {
    #[call]
    #[args(args = "*")]
    fn __call__(&mut self, args: &PyTuple) {
        println!("Start response called with {}", args);
        let header_map = HashMap::new();
        self.headers.replace(header_map);
    }
}

impl Future for StartResponse {
    type Output = HashMap<String, String>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match *self.headers.clone() {
            Some(headers) => Poll::Ready(headers),
            None => Poll::Pending,
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
    resp_sender: Sender<Vec<u8>>,
}

fn base_environ() -> HashMap<String, String> {
    let wsgi_env_vars = vec![("wsgi.url_scheme", "http"), ("HTTP_HOST", "localhost")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()));
    env::vars().into_iter().chain(wsgi_env_vars).collect()
}

fn invoke_app<'a>(
    py: Python,
    app: &'a PyAny,
    req: BussardRequest,
) -> PyResult<&'a PyAny> {
    let full_environ: &PyAny = add_per_request_environ(py, req).into();
    let sr = StartResponse {
        headers: Box::new(Option::None),
    };
    let py_sr = PyCell::new(py, sr).unwrap();
    let args = PyTuple::new(py, vec![full_environ, py_sr]);
    let resp = app.call1(args);
    println!("{:?}", resp);
    return resp;
}

async fn bussard(
    receiver: &mut Receiver<AsyncBussardRequest>,
) {
    let gil = Python::acquire_gil();
    let py = gil.python();
    let app = flaskapp(py).unwrap();

    loop {
        match receiver.recv().await {
            Some(mut req) => match invoke_app(py, app, req.req) {
                Ok(resp) => {
                    req.resp_sender
                        .send(format!("{}", resp).into_bytes())
                        .await
                        .unwrap();
                }
                Err(e) => {
                    e.print_and_set_sys_last_vars(py);
                }
            },
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

fn build_async_req(req: BussardRequest, resp_sender: Sender<Vec<u8>>) -> AsyncBussardRequest {
    AsyncBussardRequest { req, resp_sender }
}

async fn dispatch_req(
    header_map: HeaderMap,
    method: Method,
    mut sender: Sender<AsyncBussardRequest>,
) -> Result<Vec<u8>, Rejection> {
    let req = build_req(header_map, method);
    let ch = channel::<Vec<u8>>(16);
    let resp_sender = ch.0;
    let mut resp_reciever = ch.1;
    let aync_req = build_async_req(req, resp_sender);
    sender.send(aync_req).await.unwrap();
    Ok(resp_reciever.recv().await.unwrap())
}

#[tokio::main]
async fn main() {
    let mut ch = channel::<AsyncBussardRequest>(1024);
    let receiver: &mut Receiver<AsyncBussardRequest> = &mut ch.1;
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

#[cfg(test)]
mod tests {

    use crate::{flaskapp, invoke_app, BussardRequest};
    use http::{HeaderMap, Method};
    use pyo3::{Python};

    #[test]
    fn test_sync_wsgi() {
        let gil = Python::acquire_gil();
        let py = gil.python();
        let app = flaskapp(py).map_err(|e| {
            // We can't display python error type via ::std::fmt::Display,
            // so print error here manually.
            e.print_and_set_sys_last_vars(py);
        }).unwrap();

        let req = BussardRequest {
            header_map: HeaderMap::new(),
            method: Method::GET,
        };
        let resp = invoke_app(py, app, req)
            .map_err(|e| {
                // We can't display python error type via ::std::fmt::Display,
                // so print error here manually.
                e.print_and_set_sys_last_vars(py);
            })
            .unwrap();

        println!("{}", resp);
    }
}
