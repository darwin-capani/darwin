import ReactDOM from "react-dom/client";
import App from "./App";
import "./styles.css";

// No StrictMode: its dev-mode double-mount creates and destroys a second
// WebGL context and a second WebSocket at startup, which reads as a visible
// flash and trips the connection flap detector. The reducer's purity is
// guarded by the vitest suite instead.
ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(<App />);
