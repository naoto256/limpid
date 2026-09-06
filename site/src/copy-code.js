const copyIcon =
  '<svg viewBox="0 0 24 24" aria-hidden="true"><rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V4H4v12h4"/></svg>';
const checkIcon =
  '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m5 12 4 4L19 6"/></svg>';

export async function copyCode(code, button, status, clipboard) {
  button.disabled = true;
  try {
    await clipboard.writeText(code.textContent);
    button.innerHTML = checkIcon;
    button.setAttribute("aria-label", "Copied");
    status.textContent = "Copied";
  } catch {
    button.setAttribute(
      "aria-label",
      "Copy failed. Select the code and copy manually.",
    );
    status.textContent = "Copy failed — select the code and copy manually.";
  } finally {
    button.disabled = false;
  }
}

export function enhanceCodeBlocks(doc, clipboard) {
  for (const code of doc.querySelectorAll("pre > code")) {
    const pre = code.parentElement;
    if (pre.parentElement.classList.contains("code-block")) continue;
    const wrapper = doc.createElement("div");
    wrapper.className = "code-block";
    pre.before(wrapper);
    wrapper.append(pre);
    const button = doc.createElement("button");
    button.type = "button";
    button.className = "copy-code";
    button.title = "Copy code";
    button.setAttribute("aria-label", "Copy code");
    button.innerHTML = copyIcon;
    const status = doc.createElement("span");
    status.className = "copy-status";
    status.setAttribute("role", "status");
    status.setAttribute("aria-live", "polite");
    wrapper.append(button, status);
    let timer;
    button.addEventListener("click", async () => {
      clearTimeout(timer);
      await copyCode(code, button, status, clipboard);
      timer = setTimeout(() => {
        button.innerHTML = copyIcon;
        button.setAttribute("aria-label", "Copy code");
        status.textContent = "";
      }, 4000);
    });
  }
}

if (typeof document !== "undefined") {
  enhanceCodeBlocks(document, navigator.clipboard);
}
