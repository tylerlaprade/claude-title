import js from "@eslint/js";

export default [{
    files: ["src/ghostty.js"],
    ...js.configs.all,
    languageOptions: {
        sourceType: "script",
        globals: {ObjC: "readonly", $: "readonly", Ref: "readonly", escape: "readonly"}
    },
    rules: {
        ...js.configs.all.rules,
        strict: ["error", "global"],
        "new-cap": ["error", {capIsNewExceptions: ["Ref"]}],
        "one-var": "off", // Separate declarations keep native event construction readable.
        "no-magic-numbers": "off", // Native protocol constants and byte offsets are intentional.
        "no-bitwise": "off", // Apple event options are bit flags.
        "no-plusplus": "off", // Increment syntax is a style choice.
        "no-continue": "off", // Early continuation keeps the byte reader shallow.
        "max-params": "off", // Native event tuples naturally have four parts.
        "max-statements": "off" // Splitting short protocol operations would obscure their order.
    }
}];
