import { createRoot } from "react-dom/client";
import "./styles.css";
import { applySavedTheme } from "./theme";
import { App } from "./App";

// The theme picked in this browser before anything paints, so a dark choice
// on a light system never flashes light.
applySavedTheme();
createRoot(document.getElementById("root")!).render(<App />);
