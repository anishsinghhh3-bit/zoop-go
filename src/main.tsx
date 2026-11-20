import { createRoot } from "react-dom/client";
import { App, RecorderController } from "./App";
import "./styles.css";

const controller = new URLSearchParams(window.location.search).has("controller");
document.documentElement.classList.toggle("controller-mode", controller);
createRoot(document.getElementById("root")!).render(controller ? <RecorderController /> : <App />);
