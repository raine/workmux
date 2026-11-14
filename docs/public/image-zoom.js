(() => {
  const selector = "main .sl-markdown-content img";
  let preview;
  let activeTrigger;

  function imageLabel(image) {
    return `Zoom image: ${image.alt.trim()}`;
  }

  function closeOnBackdrop(event) {
    if (event.target === event.currentTarget) event.currentTarget.close();
  }

  function createDialog() {
    const dialog = document.createElement("dialog");
    dialog.className = "image-zoom-dialog";
    dialog.setAttribute("aria-label", "Image preview");

    const frame = document.createElement("div");
    frame.className = "image-zoom-frame";

    const closeButton = document.createElement("button");
    closeButton.type = "button";
    closeButton.className = "image-zoom-close";
    closeButton.setAttribute("aria-label", "Close image preview");
    closeButton.textContent = "×";
    closeButton.addEventListener("click", () => dialog.close());

    const image = document.createElement("img");
    image.className = "image-zoom-full";

    frame.append(closeButton, image);
    dialog.append(frame);
    dialog.addEventListener("click", closeOnBackdrop);
    dialog.addEventListener("close", () => {
      activeTrigger?.focus();
      activeTrigger = undefined;
    });
    document.body.append(dialog);

    return { dialog, image };
  }

  function eligible(image) {
    return (
      image.alt.trim() &&
      !image.src.toLowerCase().endsWith(".svg") &&
      !image.closest("a, button, [data-no-zoom], .image-zoom-trigger")
    );
  }

  function initImageZoom() {
    const images = Array.from(document.querySelectorAll(selector));

    for (const image of images) {
      if (!(image instanceof HTMLImageElement) || !eligible(image)) continue;

      const trigger = document.createElement("button");
      trigger.type = "button";
      trigger.className = "image-zoom-trigger";
      trigger.setAttribute("aria-label", imageLabel(image));

      image.parentNode?.insertBefore(trigger, image);
      trigger.append(image);

      trigger.addEventListener("click", () => {
        preview ??= createDialog();
        activeTrigger = trigger;
        preview.image.src = image.src;
        preview.image.srcset = image.srcset;
        preview.image.sizes = "calc(100vw - 2rem)";
        preview.image.alt = image.alt;
        preview.dialog.showModal();
      });
    }
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", initImageZoom, {
      once: true,
    });
  } else {
    initImageZoom();
  }
})();
