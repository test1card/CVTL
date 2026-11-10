import { mount } from "svelte";
import App from "./App.svelte";
export function mountApp(target) {
  mount(App, { target });
}
