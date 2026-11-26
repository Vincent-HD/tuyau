const root = document.documentElement;
const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

root.classList.add("js");

// Reveal elements as they scroll into view, staggering siblings.
const revealed = new IntersectionObserver(
  (entries) => {
    entries.forEach((entry) => {
      if (!entry.isIntersecting) {
        return;
      }
      entry.target.classList.add("in");
      revealed.unobserve(entry.target);
    });
  },
  { threshold: 0.12, rootMargin: "0px 0px -6% 0px" },
);

document.querySelectorAll(".reveal").forEach((element) => {
  const siblings = [...element.parentElement.children].filter((child) =>
    child.classList.contains("reveal"),
  );
  element.style.setProperty("--stagger", `${Math.min(siblings.indexOf(element), 8) * 70}ms`);
  revealed.observe(element);
});

// The nav gets a backdrop once the page has scrolled.
const nav = document.getElementById("nav");
const tilted = document.getElementById("tilt");

const onScroll = () => {
  nav.classList.toggle("stuck", window.scrollY > 24);
  if (!tilted || reducedMotion) {
    return;
  }
  // The main screenshot starts tilted back and straightens as it comes up.
  const { top } = tilted.getBoundingClientRect();
  const progress = Math.min(Math.max(1 - top / window.innerHeight, 0), 1);
  tilted.style.setProperty("--tilt", `${(1 - progress) * 14}deg`);
  tilted.style.setProperty("--zoom", `${0.94 + progress * 0.06}`);
};

window.addEventListener("scroll", onScroll, { passive: true });
onScroll();

// Copy buttons.
document.querySelectorAll("[data-copy]").forEach((button) => {
  const label = button.querySelector(".copy-label");
  button.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(button.dataset.copy);
      label.textContent = "copied";
    } catch {
      label.textContent = "press Ctrl+C";
    }
    button.classList.add("copied");
    setTimeout(() => {
      label.textContent = "copy";
      button.classList.remove("copied");
    }, 1600);
  });
});

// Code tabs.
document.querySelectorAll(".code").forEach((code) => {
  const tabs = code.querySelectorAll("[data-tab]");
  tabs.forEach((tab) => {
    tab.addEventListener("click", () => {
      tabs.forEach((other) => other.setAttribute("aria-selected", String(other === tab)));
      code.querySelectorAll("[data-panel]").forEach((panel) => {
        panel.hidden = panel.dataset.panel !== tab.dataset.tab;
      });
    });
  });
});

// Feature cards light up under the pointer.
document.querySelectorAll(".feature").forEach((card) => {
  card.addEventListener("pointermove", (event) => {
    const bounds = card.getBoundingClientRect();
    card.style.setProperty("--x", `${event.clientX - bounds.left}px`);
    card.style.setProperty("--y", `${event.clientY - bounds.top}px`);
  });
});
