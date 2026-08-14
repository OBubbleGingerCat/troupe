import { render } from "preact";

import { App } from "./app";
import "./styles/base.css";


const root = document.getElementById("app");

if (root === null) {
  throw new Error("diagnostics application root is missing");
}

render(<App />, root);
