// ─────────────────────────────────────────────────────────────────────────────
//  Frontend → backend API location.
//
//  • Local dev / single-host (Rust binary serves everything): leave this BLANK.
//  • Vercel-hosted frontend talking to a Railway-hosted API:
//      set API_BASE to your Railway URL, e.g.
//          window.API_BASE = "https://statement-analyzer-production.up.railway.app";
//      then redeploy the frontend on Vercel.
//
//  (You can also override per-visit with ?api=https://... in the URL.)
// ─────────────────────────────────────────────────────────────────────────────
window.API_BASE = "";
