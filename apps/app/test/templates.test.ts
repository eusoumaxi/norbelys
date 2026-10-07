import { expect, test } from "bun:test";

import {
  missingPaths,
  piecesToHtml,
  renderTemplate,
  unsupportedTags,
} from "../src/features/messages/templates";
import type { PreviewContext } from "../src/features/messages/templates";

const context = (person: Record<string, unknown>): PreviewContext => ({
  namespaces: {
    person,
    sender: { email: "santiago@EXAMPLE.COM", name: "Santiago" },
  },
  sample: false,
  snippets: false,
});

test("personalized subject and greeting use the first name, and the signature resolves its domain", () => {
  const subject = renderTemplate(
    '{{ person.given_name | trim | split(" ") | first }}, una pregunta sobre tu agenda',
    context({ given_name: "  Lisbeth Viviana Gonzalez Cortes  " })
  );
  expect(piecesToHtml(subject, (path) => path)).toBe(
    "Lisbeth, una pregunta sobre tu agenda"
  );
  const signature = renderTemplate(
    '<a href="https://{{ sender.email | split("@") | last | lower }}">{{ sender.email | split("@") | last | lower }}</a>',
    context({})
  );
  expect(piecesToHtml(signature, (path) => path)).toBe(
    '<a href="https://example.com">example.com</a>'
  );
  expect(unsupportedTags([...subject, ...signature])).toEqual([]);
});

test("greetings fall back to the institution or a neutral greeting without inventing a name", () => {
  const greeting =
    '{% if person.given_name %}Hola, {{ person.given_name | trim | split(" ") | first }}:{% elif person.company %}Hola, equipo de {{ person.company | trim }}:{% else %}Hola:{% endif %}';
  for (const [person, expected] of [
    [{ given_name: "María José Pérez" }, "Hola, María:"],
    [
      { company: "  Clínica & Salud  " },
      "Hola, equipo de Clínica &amp; Salud:",
    ],
    [{}, "Hola:"],
  ] as const) {
    const pieces = renderTemplate(greeting, context(person));
    expect(piecesToHtml(pieces, (path) => path)).toBe(expected);
    expect(missingPaths(pieces)).toEqual([]);
  }
});

test("sequence filters preserve Unicode, missing values and empty-sequence defaults", () => {
  const c = context({ given_name: "  María\tJosé  " });
  expect(renderTemplate("{{ person.given_name | split | last }}", c)).toEqual([
    { kind: "value", text: "José" },
  ]);
  expect(renderTemplate('{{ "😀a" | first }}', c)).toEqual([
    { kind: "value", text: "😀" },
  ]);
  expect(
    renderTemplate('{{ " " | split | first | default("there") }}', c)
  ).toEqual([{ kind: "value", text: "there" }]);
  expect(
    missingPaths(renderTemplate('{{ person.company | split(" ") | first }}', c))
  ).toEqual(["person.company"]);
});

test("unsupported syntax stays visible and is identified as an incomplete preview", () => {
  const source = "{{ person.given_name | unsupported_filter }}";
  expect(
    unsupportedTags(renderTemplate(source, context({ given_name: "Ana" })))
  ).toEqual([source]);
  // A person's literal braces are data, not an unsupported template expression.
  expect(
    unsupportedTags(
      renderTemplate(
        "{{ person.company }}",
        context({ company: "{{example}}" })
      )
    )
  ).toEqual([]);
});
