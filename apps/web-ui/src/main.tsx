import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./app";
import { boot } from "./store/app";
import "./styles/app.css";

boot(); // auto-connect when a token is present (fragment or stored)

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>
);

if ("serviceWorker" in navigator && location.protocol.startsWith("http")) {
  navigator.serviceWorker.register("/sw.js").catch(() => {});
}
