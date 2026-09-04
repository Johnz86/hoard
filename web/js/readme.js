import { el } from "./dom.js";

export function renderCard(readme, revision) {
    if (!readme) return null;
    const frontmatter = readme.frontmatter || [];
    return el(
        "div",
        { class: "card-wrap" },
        el(
            "article",
            { class: "paper" },
            el(
                "div",
                { class: "paper-bar" },
                el("span", { class: "paper-glyph", text: "□" }),
                el("span", { text: "README.md" }),
                el("span", { class: "paper-rev", text: `· ${revision || "main"}` }),
                readme.generated ? el("span", { class: "paper-gen", text: "· GENERATED" }) : null,
                el("span", { class: "paper-meta", text: readme.meta || "" })
            ),
            el(
                "div",
                { class: "paper-body" },
                frontmatter.length
                    ? el(
                          "div",
                          { class: "frontmatter" },
                          el("div", { class: "frontmatter-title", text: "YAML FRONTMATTER" }),
                          frontmatter.map((entry) =>
                              el(
                                  "div",
                                  null,
                                  el("span", { class: "fm-key", text: `${entry.key}: ` }),
                                  String(entry.value ?? "")
                              )
                          )
                      )
                    : null,
                el("div", { class: "prose", html: readme.html || "" })
            )
        )
    );
}
