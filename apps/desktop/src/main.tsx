import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";

import { QueryClientProvider } from "@tanstack/react-query";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter } from "react-router-dom";

import { App } from "./app/App";
import { queryClient } from "./lib/query-client";
import "./index.css";

const container = document.getElementById("root");
if (!container) {
  throw new Error("the #root element is missing");
}

createRoot(container).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <BrowserRouter>
        <App />
      </BrowserRouter>
    </QueryClientProvider>
  </StrictMode>,
);
