import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import Capsule from "./Capsule";
import "./styles.css";

const params = new URLSearchParams(window.location.search);
const isCapsule = params.get("window") === "capsule";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>{isCapsule ? <Capsule /> : <App />}</React.StrictMode>
);
