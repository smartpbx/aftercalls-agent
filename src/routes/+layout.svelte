<script lang="ts">
  import { page } from "$app/state";
  let { children } = $props();
</script>

<nav>
  <a href="/" class:active={page.url.pathname === "/"}>Record</a>
  <a href="/calls" class:active={page.url.pathname.startsWith("/calls")}>Calls</a>
</nav>

<div class="content">
  {@render children()}
</div>

<style>
  :global(:root) {
    font-family: Inter, Avenir, Helvetica, Arial, sans-serif;
    font-size: 16px;
    color: #f6f6f6;
    background-color: #1a1a1a;
  }
  :global(body) {
    margin: 0;
    min-height: 100vh;
  }

  nav {
    display: flex;
    gap: 0.5rem;
    padding: 0.75rem 1.25rem;
    border-bottom: 1px solid #2a2a2a;
    background-color: #151515;
    position: sticky;
    top: 0;
    z-index: 10;
  }

  nav a {
    padding: 0.35rem 0.9rem;
    border-radius: 999px;
    text-decoration: none;
    color: #a0a0a0;
    font-size: 0.9rem;
    font-weight: 500;
    transition: color 0.15s, background-color 0.15s;
  }

  nav a:hover {
    color: #f6f6f6;
  }

  nav a.active {
    color: #f6f6f6;
    background-color: #2a2a2a;
  }

  .content {
    min-height: calc(100vh - 3rem);
  }
</style>
