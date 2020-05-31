from flask import Flask
import pandas as pd


def make_app() -> Flask:
    app = Flask(__name__)

    @app.route('/')
    def hello_world():
        return 'Hello, World!'

    return app


def make_df() -> pd.DataFrame:
    return pd.DataFrame([{"int": 5, "string": "foo", "float": 1.0 + 2.0}])


def main():
    app = make_app()
    app.run()


if __name__ == "__main__":
    main()
