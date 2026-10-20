#!/usr/bin/env python3
"""Check the actual bundle template used by Launch Services and Photos' Edit With."""

import pathlib
import plistlib
import re
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]


class DocumentTypes(unittest.TestCase):
    def test_importable_photos_are_registered_as_editable_documents(self):
        with (ROOT / "packaging/macos/Info.plist.in").open("rb") as source:
            plist = plistlib.load(source)
        documents = plist.get("CFBundleDocumentTypes", [])
        editors = [entry for entry in documents if entry.get("CFBundleTypeRole") == "Editor"]
        self.assertTrue(editors, "Photos needs an Editor document handler, not just UTI declarations")
        extensions = {ext for entry in editors for ext in entry.get("CFBundleTypeExtensions", [])}
        source = (ROOT / "crates/engine/src/import.rs").read_text()
        declaration = source.split("pub const EXTENSIONS: &[&str] = &[", 1)[1].split("];", 1)[0]
        declaration = re.sub(r"//[^\n]*", "", declaration)
        self.assertEqual(extensions, set(re.findall(r'"([a-z0-9]+)"', declaration)))
        types = {uti for entry in editors for uti in entry.get("LSItemContentTypes", [])}
        self.assertTrue({"public.camera-raw-image", "public.tiff", "public.jpeg"} <= types)
        for entry in editors:
            self.assertEqual(entry.get("LSHandlerRank"), "Alternate", "do not take over the default photo app")


if __name__ == "__main__":
    unittest.main()
