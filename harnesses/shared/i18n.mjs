// Runtime message catalog for the Node harnesses.
//
// Messages live in `locales/<locale>.json` as flat `key -> template` maps and
// are selected from `LC_ALL` / `LC_MESSAGES` / `LANG`. A `zh*` locale uses
// `zh-CN`; everything else uses `en`. Templates use `{name}` placeholders.

import fs from "node:fs"

const catalogs = {
  en: JSON.parse(fs.readFileSync(new URL("./locales/en.json", import.meta.url), "utf8")),
  "zh-CN": JSON.parse(fs.readFileSync(new URL("./locales/zh-CN.json", import.meta.url), "utf8")),
}

let active = detectLocale()

export function detectLocale() {
  const value =
    (process.env.LC_ALL || "").trim() ||
    (process.env.LC_MESSAGES || "").trim() ||
    (process.env.LANG || "").trim()
  return value.toLowerCase().startsWith("zh") ? "zh-CN" : "en"
}

export function setLocale(locale) {
  active = String(locale ?? "").toLowerCase().startsWith("zh") ? "zh-CN" : "en"
  return active
}

export function locale() {
  return active
}

export function t(key, args = {}) {
  const template = catalogs[active]?.[key] ?? catalogs.en[key] ?? key
  return String(template).replace(/\{(\w+)\}/g, (match, name) =>
    Object.hasOwn(args, name) ? String(args[name]) : match,
  )
}
