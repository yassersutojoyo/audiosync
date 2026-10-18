#!/usr/bin/env python3
"""Write THIRD-PARTY-NOTICES.txt for a Windows build of AudioSync.

    python3 -I scripts/windows_notices.py <target triple> [--output <file>]

The file covers everything compiled into audiosync.exe for that target:
AudioSync's own MIT code (the app and audiosync-core), the Rust standard
library, Slint (used under its royalty-free license), the fonts and icons the
interface embeds, and every crate in the app's normal dependency graph, read
from windows/Cargo.lock. For each crate it gives the name, version, license
expression and repository, then the full text of every license or notice file
in the crate's package (LICENSE*, LICENCE*, COPYING*, NOTICE*, COPYRIGHT*).
A crate without such a file gets the standard text of its license with the
crate's authors as the copyright holders. Identical texts are written once and
referenced afterwards.

The graph comes from `cargo metadata --filter-platform`, walked from
audiosync-windows over normal dependencies and narrowed to the crates
`cargo tree --edges normal,no-proc-macro --target` reports: cargo metadata
resolves features for every platform at once, so on its own it would also list
crates that only other platforms build (Skia, for instance). Build and
development dependencies and procedural macros run on the build machine and
aren't part of the executable, so they're left out; that also keeps the file
the same whichever system generates it. Slint's compiler, which turns the
interface into code, is covered by Slint's own section.

Some crates contain code from other projects whose notice the crate's own
license files don't carry (AccessKit's Chromium-derived code, for instance).
SUPPLEMENTS adds those notices. The script scans each compiled crate's source
for a reference to a license file ("... found in the LICENSE.chromium file")
or for third-party code it knows needs a notice, and fails when the package
doesn't ship that file and no supplement covers it.

The script also fails when a crate's license expression is not covered by
windows/deny.toml (its allow list, plus the crate's own entry under
exceptions), so the notices and the CI license check can't drift apart. It
needs only cargo, rustc (whose rustc component ships the standard library's
copyright notices) and the Python standard library.

The default output is THIRD-PARTY-NOTICES.txt next to audiosync.exe:
windows/target/<target>/release/ (or under CARGO_TARGET_DIR when it is set).
"""
import argparse
import html.parser
import json
import os
import re
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
WINDOWS = os.path.join(REPO, "windows")
ROOT_PACKAGE = "audiosync-windows"
NOTICE_PREFIXES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "COPYRIGHT")
# Slint's crates are "GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR
# LicenseRef-Slint-Software-3.0"; AudioSync takes the royalty-free option.
SLINT_ELECTED = "LicenseRef-Slint-Royalty-free-2.0"
RULE = "=" * 78
THIN_RULE = "-" * 78


class NoticeError(Exception):
    pass


# Standard license texts for crates whose package has no license file. The
# copyright line names the crate's authors ({holders}).

MIT = """\
MIT License

Copyright (c) {holders}

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
"""

BSD_2_CLAUSE = """\
BSD 2-Clause License

Copyright (c) {holders}

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
"""

BSD_3_CLAUSE = """\
BSD 3-Clause License

Copyright (c) {holders}

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its
   contributors may be used to endorse or promote products derived from
   this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
"""

ISC = """\
ISC License

Copyright (c) {holders}

Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
"""

ZLIB = """\
zlib License

Copyright (c) {holders}

This software is provided 'as-is', without any express or implied
warranty. In no event will the authors be held liable for any damages
arising from the use of this software.

Permission is granted to anyone to use this software for any purpose,
including commercial applications, and to alter it and redistribute it
freely, subject to the following restrictions:

1. The origin of this software must not be misrepresented; you must not
   claim that you wrote the original software. If you use this software
   in a product, an acknowledgment in the product documentation would be
   appreciated but is not required.
2. Altered source versions must be plainly marked as such, and must not be
   misrepresented as being the original software.
3. This notice may not be removed or altered from any source distribution.
"""

BSL_1_0 = """\
Copyright (c) {holders}

Boost Software License - Version 1.0 - August 17th, 2003

Permission is hereby granted, free of charge, to any person or organization
obtaining a copy of the software and accompanying documentation covered by
this license (the "Software") to use, reproduce, display, distribute,
execute, and transmit the Software, and to prepare derivative works of the
Software, and to permit third-parties to whom the Software is furnished to
do so, all subject to the following:

The copyright notices in the Software and this entire statement, including
the above license grant, this restriction and the following disclaimer,
must be included in all copies of the Software, in whole or in part, and
all derivative works of the Software, unless such copies or derivative
works are solely in the form of machine-executable object code generated by
a source language processor.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE, TITLE AND NON-INFRINGEMENT. IN NO EVENT
SHALL THE COPYRIGHT HOLDERS OR ANYONE DISTRIBUTING THE SOFTWARE BE LIABLE
FOR ANY DAMAGES OR OTHER LIABILITY, WHETHER IN CONTRACT, TORT OR OTHERWISE,
ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
"""

APACHE_2_0 = """\
Copyright (c) {holders}

Apache License
Version 2.0, January 2004
http://www.apache.org/licenses/

TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION

1. Definitions.

"License" shall mean the terms and conditions for use, reproduction, and distribution as defined by Sections 1 through 9 of this document.

"Licensor" shall mean the copyright owner or entity authorized by the copyright owner that is granting the License.

"Legal Entity" shall mean the union of the acting entity and all other entities that control, are controlled by, or are under common control with that entity. For the purposes of this definition, "control" means (i) the power, direct or indirect, to cause the direction or management of such entity, whether by contract or otherwise, or (ii) ownership of fifty percent (50%) or more of the outstanding shares, or (iii) beneficial ownership of such entity.

"You" (or "Your") shall mean an individual or Legal Entity exercising permissions granted by this License.

"Source" form shall mean the preferred form for making modifications, including but not limited to software source code, documentation source, and configuration files.

"Object" form shall mean any form resulting from mechanical transformation or translation of a Source form, including but not limited to compiled object code, generated documentation, and conversions to other media types.

"Work" shall mean the work of authorship, whether in Source or Object form, made available under the License, as indicated by a copyright notice that is included in or attached to the work (an example is provided in the Appendix below).

"Derivative Works" shall mean any work, whether in Source or Object form, that is based on (or derived from) the Work and for which the editorial revisions, annotations, elaborations, or other modifications represent, as a whole, an original work of authorship. For the purposes of this License, Derivative Works shall not include works that remain separable from, or merely link (or bind by name) to the interfaces of, the Work and Derivative Works thereof.

"Contribution" shall mean any work of authorship, including the original version of the Work and any modifications or additions to that Work or Derivative Works thereof, that is intentionally submitted to Licensor for inclusion in the Work by the copyright owner or by an individual or Legal Entity authorized to submit on behalf of the copyright owner. For the purposes of this definition, "submitted" means any form of electronic, verbal, or written communication sent to the Licensor or its representatives, including but not limited to communication on electronic mailing lists, source code control systems, and issue tracking systems that are managed by, or on behalf of, the Licensor for the purpose of discussing and improving the Work, but excluding communication that is conspicuously marked or otherwise designated in writing by the copyright owner as "Not a Contribution."

"Contributor" shall mean Licensor and any individual or Legal Entity on behalf of whom a Contribution has been received by Licensor and subsequently incorporated within the Work.

2. Grant of Copyright License. Subject to the terms and conditions of this License, each Contributor hereby grants to You a perpetual, worldwide, non-exclusive, no-charge, royalty-free, irrevocable copyright license to reproduce, prepare Derivative Works of, publicly display, publicly perform, sublicense, and distribute the Work and such Derivative Works in Source or Object form.

3. Grant of Patent License. Subject to the terms and conditions of this License, each Contributor hereby grants to You a perpetual, worldwide, non-exclusive, no-charge, royalty-free, irrevocable (except as stated in this section) patent license to make, have made, use, offer to sell, sell, import, and otherwise transfer the Work, where such license applies only to those patent claims licensable by such Contributor that are necessarily infringed by their Contribution(s) alone or by combination of their Contribution(s) with the Work to which such Contribution(s) was submitted. If You institute patent litigation against any entity (including a cross-claim or counterclaim in a lawsuit) alleging that the Work or a Contribution incorporated within the Work constitutes direct or contributory patent infringement, then any patent licenses granted to You under this License for that Work shall terminate as of the date such litigation is filed.

4. Redistribution. You may reproduce and distribute copies of the Work or Derivative Works thereof in any medium, with or without modifications, and in Source or Object form, provided that You meet the following conditions:

     (a) You must give any other recipients of the Work or Derivative Works a copy of this License; and

     (b) You must cause any modified files to carry prominent notices stating that You changed the files; and

     (c) You must retain, in the Source form of any Derivative Works that You distribute, all copyright, patent, trademark, and attribution notices from the Source form of the Work, excluding those notices that do not pertain to any part of the Derivative Works; and

     (d) If the Work includes a "NOTICE" text file as part of its distribution, then any Derivative Works that You distribute must include a readable copy of the attribution notices contained within such NOTICE file, excluding those notices that do not pertain to any part of the Derivative Works, in at least one of the following places: within a NOTICE text file distributed as part of the Derivative Works; within the Source form or documentation, if provided along with the Derivative Works; or, within a display generated by the Derivative Works, if and wherever such third-party notices normally appear. The contents of the NOTICE file are for informational purposes only and do not modify the License. You may add Your own attribution notices within Derivative Works that You distribute, alongside or as an addendum to the NOTICE text from the Work, provided that such additional attribution notices cannot be construed as modifying the License.

     You may add Your own copyright statement to Your modifications and may provide additional or different license terms and conditions for use, reproduction, or distribution of Your modifications, or for any such Derivative Works as a whole, provided Your use, reproduction, and distribution of the Work otherwise complies with the conditions stated in this License.

5. Submission of Contributions. Unless You explicitly state otherwise, any Contribution intentionally submitted for inclusion in the Work by You to the Licensor shall be under the terms and conditions of this License, without any additional terms or conditions. Notwithstanding the above, nothing herein shall supersede or modify the terms of any separate license agreement you may have executed with Licensor regarding such Contributions.

6. Trademarks. This License does not grant permission to use the trade names, trademarks, service marks, or product names of the Licensor, except as required for reasonable and customary use in describing the origin of the Work and reproducing the content of the NOTICE file.

7. Disclaimer of Warranty. Unless required by applicable law or agreed to in writing, Licensor provides the Work (and each Contributor provides its Contributions) on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied, including, without limitation, any warranties or conditions of TITLE, NON-INFRINGEMENT, MERCHANTABILITY, or FITNESS FOR A PARTICULAR PURPOSE. You are solely responsible for determining the appropriateness of using or redistributing the Work and assume any risks associated with Your exercise of permissions under this License.

8. Limitation of Liability. In no event and under no legal theory, whether in tort (including negligence), contract, or otherwise, unless required by applicable law (such as deliberate and grossly negligent acts) or agreed to in writing, shall any Contributor be liable to You for damages, including any direct, indirect, special, incidental, or consequential damages of any character arising as a result of this License or out of the use or inability to use the Work (including but not limited to damages for loss of goodwill, work stoppage, computer failure or malfunction, or any and all other commercial damages or losses), even if such Contributor has been advised of the possibility of such damages.

9. Accepting Warranty or Additional Liability. While redistributing the Work or Derivative Works thereof, You may choose to offer, and charge a fee for, acceptance of support, warranty, indemnity, or other liability obligations and/or rights consistent with this License. However, in accepting such obligations, You may act only on Your own behalf and on Your sole responsibility, not on behalf of any other Contributor, and only if You agree to indemnify, defend, and hold each Contributor harmless for any liability incurred by, or claims asserted against, such Contributor by reason of your accepting any such warranty or additional liability.

END OF TERMS AND CONDITIONS

APPENDIX: How to apply the Apache License to your work.

To apply the Apache License to your work, attach the following boilerplate notice, with the fields enclosed by brackets "[]" replaced with your own identifying information. (Don't include the brackets!)  The text should be enclosed in the appropriate comment syntax for the file format. We also recommend that a file or class name and description of purpose be included on the same "printed page" as the copyright notice for easier identification within third-party archives.

Copyright [yyyy] [name of copyright owner]

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
"""

UNICODE_3_0 = """\
Copyright (c) {holders}

UNICODE LICENSE V3

COPYRIGHT AND PERMISSION NOTICE

Copyright © 1991-2024 Unicode, Inc.

NOTICE TO USER: Carefully read the following legal agreement. BY
DOWNLOADING, INSTALLING, COPYING OR OTHERWISE USING DATA FILES, AND/OR
SOFTWARE, YOU UNEQUIVOCALLY ACCEPT, AND AGREE TO BE BOUND BY, ALL OF THE
TERMS AND CONDITIONS OF THIS AGREEMENT. IF YOU DO NOT AGREE, DO NOT
DOWNLOAD, INSTALL, COPY, DISTRIBUTE OR USE THE DATA FILES OR SOFTWARE.

Permission is hereby granted, free of charge, to any person obtaining a
copy of data files and any associated documentation (the "Data Files") or
software and any associated documentation (the "Software") to deal in the
Data Files or Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, and/or sell
copies of the Data Files or Software, and to permit persons to whom the
Data Files or Software are furnished to do so, provided that either (a)
this copyright and permission notice appear with all copies of the Data
Files or Software, or (b) this copyright and permission notice appear in
associated Documentation.

THE DATA FILES AND SOFTWARE ARE PROVIDED "AS IS", WITHOUT WARRANTY OF ANY
KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT OF
THIRD PARTY RIGHTS.

IN NO EVENT SHALL THE COPYRIGHT HOLDER OR HOLDERS INCLUDED IN THIS NOTICE
BE LIABLE FOR ANY CLAIM, OR ANY SPECIAL INDIRECT OR CONSEQUENTIAL DAMAGES,
OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS,
WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION,
ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THE DATA
FILES OR SOFTWARE.

Except as contained in this notice, the name of a copyright holder shall
not be used in advertising or otherwise to promote the sale, use or other
dealings in these Data Files or Software without prior written
authorization of the copyright holder.
"""

CC0_1_0 = """\
Author: {holders}

Creative Commons Legal Code

CC0 1.0 Universal

    CREATIVE COMMONS CORPORATION IS NOT A LAW FIRM AND DOES NOT PROVIDE
    LEGAL SERVICES. DISTRIBUTION OF THIS DOCUMENT DOES NOT CREATE AN
    ATTORNEY-CLIENT RELATIONSHIP. CREATIVE COMMONS PROVIDES THIS
    INFORMATION ON AN "AS-IS" BASIS. CREATIVE COMMONS MAKES NO WARRANTIES
    REGARDING THE USE OF THIS DOCUMENT OR THE INFORMATION OR WORKS
    PROVIDED HEREUNDER, AND DISCLAIMS LIABILITY FOR DAMAGES RESULTING FROM
    THE USE OF THIS DOCUMENT OR THE INFORMATION OR WORKS PROVIDED
    HEREUNDER.

Statement of Purpose

The laws of most jurisdictions throughout the world automatically confer
exclusive Copyright and Related Rights (defined below) upon the creator
and subsequent owner(s) (each and all, an "owner") of an original work of
authorship and/or a database (each, a "Work").

Certain owners wish to permanently relinquish those rights to a Work for
the purpose of contributing to a commons of creative, cultural and
scientific works ("Commons") that the public can reliably and without fear
of later claims of infringement build upon, modify, incorporate in other
works, reuse and redistribute as freely as possible in any form whatsoever
and for any purposes, including without limitation commercial purposes.
These owners may contribute to the Commons to promote the ideal of a free
culture and the further production of creative, cultural and scientific
works, or to gain reputation or greater distribution for their Work in
part through the use and efforts of others.

For these and/or other purposes and motivations, and without any
expectation of additional consideration or compensation, the person
associating CC0 with a Work (the "Affirmer"), to the extent that he or she
is an owner of Copyright and Related Rights in the Work, voluntarily
elects to apply CC0 to the Work and publicly distribute the Work under its
terms, with knowledge of his or her Copyright and Related Rights in the
Work and the meaning and intended legal effect of CC0 on those rights.

1. Copyright and Related Rights. A Work made available under CC0 may be
protected by copyright and related or neighboring rights ("Copyright and
Related Rights"). Copyright and Related Rights include, but are not
limited to, the following:

  i. the right to reproduce, adapt, distribute, perform, display,
     communicate, and translate a Work;
 ii. moral rights retained by the original author(s) and/or performer(s);
iii. publicity and privacy rights pertaining to a person's image or
     likeness depicted in a Work;
 iv. rights protecting against unfair competition in regards to a Work,
     subject to the limitations in paragraph 4(a), below;
  v. rights protecting the extraction, dissemination, use and reuse of data
     in a Work;
 vi. database rights (such as those arising under Directive 96/9/EC of the
     European Parliament and of the Council of 11 March 1996 on the legal
     protection of databases, and under any national implementation
     thereof, including any amended or successor version of such
     directive); and
vii. other similar, equivalent or corresponding rights throughout the
     world based on applicable law or treaty, and any national
     implementations thereof.

2. Waiver. To the greatest extent permitted by, but not in contravention
of, applicable law, Affirmer hereby overtly, fully, permanently,
irrevocably and unconditionally waives, abandons, and surrenders all of
Affirmer's Copyright and Related Rights and associated claims and causes
of action, whether now known or unknown (including existing as well as
future claims and causes of action), in the Work (i) in all territories
worldwide, (ii) for the maximum duration provided by applicable law or
treaty (including future time extensions), (iii) in any current or future
medium and for any number of copies, and (iv) for any purpose whatsoever,
including without limitation commercial, advertising or promotional
purposes (the "Waiver"). Affirmer makes the Waiver for the benefit of each
member of the public at large and to the detriment of Affirmer's heirs and
successors, fully intending that such Waiver shall not be subject to
revocation, rescission, cancellation, termination, or any other legal or
equitable action to disrupt the quiet enjoyment of the Work by the public
as contemplated by Affirmer's express Statement of Purpose.

3. Public License Fallback. Should any part of the Waiver for any reason
be judged legally invalid or ineffective under applicable law, then the
Waiver shall be preserved to the maximum extent permitted taking into
account Affirmer's express Statement of Purpose. In addition, to the
extent the Waiver is so judged Affirmer hereby grants to each affected
person a royalty-free, non transferable, non sublicensable, non exclusive,
irrevocable and unconditional license to exercise Affirmer's Copyright and
Related Rights in the Work (i) in all territories worldwide, (ii) for the
maximum duration provided by applicable law or treaty (including future
time extensions), (iii) in any current or future medium and for any number
of copies, and (iv) for any purpose whatsoever, including without
limitation commercial, advertising or promotional purposes (the
"License"). The License shall be deemed effective as of the date CC0 was
applied by Affirmer to the Work. Should any part of the License for any
reason be judged legally invalid or ineffective under applicable law, such
partial invalidity or ineffectiveness shall not invalidate the remainder
of the License, and in such case Affirmer hereby affirms that he or she
will not (i) exercise any of his or her remaining Copyright and Related
Rights in the Work or (ii) assert any associated claims and causes of
action with respect to the Work, in either case contrary to Affirmer's
express Statement of Purpose.

4. Limitations and Disclaimers.

 a. No trademark or patent rights held by Affirmer are waived, abandoned,
    surrendered, licensed or otherwise affected by this document.
 b. Affirmer offers the Work as-is and makes no representations or
    warranties of any kind concerning the Work, express, implied,
    statutory or otherwise, including without limitation warranties of
    title, merchantability, fitness for a particular purpose, non
    infringement, or the absence of latent or other defects, accuracy, or
    the present or absence of errors, whether or not discoverable, all to
    the greatest extent permissible under applicable law.
 c. Affirmer disclaims responsibility for clearing rights of other persons
    that may apply to the Work or any use thereof, including without
    limitation any person's Copyright and Related Rights in the Work.
    Further, Affirmer disclaims responsibility for obtaining any necessary
    consents, permissions or other rights required for any use of the
    Work.
 d. Affirmer understands and acknowledges that Creative Commons is not a
    party to this document and has no duty or obligation with respect to
    this CC0 or use of the Work.
"""

TEMPLATES = {
    "MIT": MIT,
    "Apache-2.0": APACHE_2_0,
    "BSD-2-Clause": BSD_2_CLAUSE,
    "BSD-3-Clause": BSD_3_CLAUSE,
    "ISC": ISC,
    "Zlib": ZLIB,
    "Unicode-3.0": UNICODE_3_0,
    "BSL-1.0": BSL_1_0,
    "CC0-1.0": CC0_1_0,
}


def run(*cmd):
    try:
        return subprocess.run(
            list(cmd), cwd=WINDOWS, check=True, stdout=subprocess.PIPE,
            universal_newlines=True, encoding="utf-8",
        ).stdout
    except (OSError, subprocess.CalledProcessError) as error:
        raise NoticeError("`{}` failed: {}".format(" ".join(cmd), error))


def clean_text(text):
    """Normalizes line endings and trailing spaces, drops a BOM and blank
    lines at either end."""
    text = text.lstrip("\ufeff").replace("\r\n", "\n").replace("\r", "\n")
    lines = [line.rstrip() for line in text.split("\n")]
    return "\n".join(lines).strip("\n")


def read_text(path):
    with open(path, "rb") as handle:
        return clean_text(handle.read().decode("utf-8", errors="replace"))


# deny.toml and SPDX expressions ------------------------------------------------

def strip_toml_comment(line):
    out, quote = [], None
    for char in line:
        if quote:
            if char == quote:
                quote = None
        elif char in "\"'":
            quote = char
        elif char == "#":
            break
        out.append(char)
    return "".join(out)


def licenses_array(key):
    """The raw text of an array in the [licenses] section of windows/deny.toml,
    or None (no TOML parser in the standard library before Python 3.11)."""
    section, collected, depth = None, None, 0
    with open(os.path.join(WINDOWS, "deny.toml"), encoding="utf-8") as handle:
        for raw in handle:
            line = strip_toml_comment(raw).strip()
            if collected is None:
                header = re.match(r"^\[([^\[\]]+)\]$", line)
                if header:
                    section = header.group(1).strip()
                    continue
                if section != "licenses" or not re.match(r"^{}\s*=".format(key), line):
                    continue
                collected = []
                line = line.split("=", 1)[1]
            collected.append(line)
            unquoted = re.sub(r'"[^"]*"', "", line)
            depth += unquoted.count("[") - unquoted.count("]")
            if depth <= 0:
                break
    return None if collected is None else " ".join(collected)


class LicensePolicy:
    """The [licenses] allow list of windows/deny.toml and its per-crate
    exceptions ({ crate = "name" or "name@version", allow = [...] })."""

    def __init__(self):
        allow = licenses_array("allow")
        if allow is None:
            raise NoticeError("windows/deny.toml has no [licenses] allow list")
        self.allowed = set(re.findall(r'"([^"]+)"', allow))
        if not self.allowed:
            raise NoticeError("the allow list in windows/deny.toml is empty")
        self.exceptions = []
        for entry in re.findall(r"\{([^{}]*)\}", licenses_array("exceptions") or ""):
            spec = re.search(r'\b(?:crate|name)\s*=\s*"([^"]+)"', entry)
            licenses = re.search(r"\ballow\s*=\s*\[([^\]]*)\]", entry)
            if not spec or not licenses:
                raise NoticeError("can't read the exception {{{}}} in windows/deny.toml".format(entry.strip()))
            name, _, version = spec.group(1).partition("@")
            self.exceptions.append((name, version or None, set(re.findall(r'"([^"]+)"', licenses.group(1)))))

    def for_package(self, package):
        allowed = set(self.allowed)
        for name, version, licenses in self.exceptions:
            if name == package["name"] and version in (None, package["version"]):
                allowed |= licenses
        return allowed


class Expression:
    """A parsed SPDX license expression (with the legacy "/" for OR)."""

    def __init__(self, text):
        self.text = text
        spaced = text.replace("/", " OR ").replace("(", " ( ").replace(")", " ) ")
        self.tokens = spaced.split()
        if not self.tokens:
            raise NoticeError("empty license expression")
        self.position = 0
        self.ids = []
        self.tree = self._or()
        if self.position != len(self.tokens):
            raise NoticeError("can't parse license expression '{}'".format(text))

    def _peek(self):
        return self.tokens[self.position] if self.position < len(self.tokens) else None

    def _take(self):
        token = self._peek()
        self.position += 1
        return token

    def _or(self):
        terms = [self._and()]
        while self._peek() == "OR":
            self._take()
            terms.append(self._and())
        return ("OR", terms) if len(terms) > 1 else terms[0]

    def _and(self):
        terms = [self._atom()]
        while self._peek() == "AND":
            self._take()
            terms.append(self._atom())
        return ("AND", terms) if len(terms) > 1 else terms[0]

    def _atom(self):
        token = self._take()
        if token == "(":
            tree = self._or()
            if self._take() != ")":
                raise NoticeError("unbalanced parentheses in '{}'".format(self.text))
            return tree
        if token is None or token in ("AND", "OR", "WITH", ")") or not re.match(r"^[A-Za-z0-9.+:-]+$", token):
            raise NoticeError("can't parse license expression '{}'".format(self.text))
        if self._peek() == "WITH":
            # Covered only when deny.toml names the whole "<id> WITH <exception>";
            # it has no standard text here, so the package must ship its license.
            self._take()
            exception = self._take()
            if exception is None or not re.match(r"^[A-Za-z0-9.+:-]+$", exception):
                raise NoticeError("can't parse license expression '{}'".format(self.text))
            return ("WITH", "{} WITH {}".format(token, exception))
        self.ids.append(token)
        return ("ID", token)

    def covered_by(self, allowed):
        def evaluate(node):
            kind, value = node
            if kind in ("ID", "WITH"):
                return value in allowed
            results = [evaluate(child) for child in value]
            return any(results) if kind == "OR" else all(results)
        return evaluate(self.tree)


# Cargo ---------------------------------------------------------------------------

def dependency_packages(target):
    """The packages compiled into audiosync.exe for the target, root first."""
    metadata = json.loads(run(
        "cargo", "metadata", "--format-version", "1", "--locked",
        "--filter-platform", target,
    ))
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    roots = [pid for pid in metadata["workspace_members"]
             if packages[pid]["name"] == ROOT_PACKAGE]
    if len(roots) != 1:
        raise NoticeError("{} isn't the workspace member in windows/".format(ROOT_PACKAGE))

    reachable, stack = [], [roots[0]]
    while stack:
        pid = stack.pop()
        if pid in reachable:
            continue
        reachable.append(pid)
        for dep in nodes[pid]["deps"]:
            if any(kind["kind"] is None for kind in dep["dep_kinds"]):
                stack.append(dep["pkg"])

    # cargo metadata unifies features across platforms; cargo tree resolves
    # them for this target the way cargo build does, and leaves out what only
    # runs at build time.
    built = set()
    tree = run("cargo", "tree", "--locked", "--target", target,
               "--edges", "normal,no-proc-macro", "--prefix", "none")
    for line in tree.splitlines():
        match = re.match(r"^(\S+) v(\S+)", line)
        if match:
            built.add((match.group(1), match.group(2)))
    by_key = {(packages[pid]["name"], packages[pid]["version"]): pid for pid in reachable}
    missing = sorted("{} {}".format(*key) for key in built if key not in by_key)
    if missing:
        raise NoticeError("cargo tree lists crates outside the metadata graph: "
                          + ", ".join(missing))
    if (ROOT_PACKAGE, packages[roots[0]]["version"]) not in built:
        raise NoticeError("cargo tree doesn't list {}".format(ROOT_PACKAGE))
    compiled = [packages[pid] for pid in reachable
                if (packages[pid]["name"], packages[pid]["version"]) in built]
    return compiled, [packages[pid] for pid in reachable]


def notice_files(package, expression):
    """(label, path) of every license or notice file in a package."""
    directory = os.path.dirname(package["manifest_path"])
    rejected = set(expression.ids) - {SLINT_ELECTED} if SLINT_ELECTED in expression.ids else set()
    found = []
    for name in sorted(os.listdir(directory), key=str.lower):
        if not name.upper().startswith(NOTICE_PREFIXES):
            continue
        path = os.path.join(directory, name)
        if os.path.isfile(path):
            found.append((name, path))
        elif os.path.isdir(path):
            for inner in sorted(os.listdir(path), key=str.lower):
                # A REUSE LICENSES/ folder: skip the options AudioSync doesn't take.
                if os.path.splitext(inner)[0] in rejected:
                    continue
                if os.path.isfile(os.path.join(path, inner)):
                    found.append((name + "/" + inner, os.path.join(path, inner)))
    declared = package.get("license_file")
    if declared:
        path = os.path.normpath(os.path.join(directory, declared))
        if os.path.isfile(path) and path not in [p for _, p in found]:
            found.append((declared, path))
    return found


def holders(package):
    authors = [re.sub(r"\s*<[^>]*>", "", author).strip() for author in package.get("authors") or []]
    authors = [author for author in authors if author]
    return ", ".join(authors) if authors else "the {} authors".format(package["name"])


# Code from other projects inside crates -----------------------------------------

SOURCE_EXTENSIONS = (".rs", ".c", ".h", ".cc", ".cpp", ".hpp", ".inc", ".slint")
_GAP = rb"[\s/*#!]+"  # spaces and comment markers between words of a comment
# "... governed by a BSD-style license that can be found in the LICENSE.x file."
LICENSE_REFERENCE = re.compile(
    rb"\bfound" + _GAP + rb"in" + _GAP + rb"(?:the" + _GAP + rb")?"
    rb"((?:licen[cs]e|copying|notice|copyright)[A-Za-z0-9_.-]*)" + _GAP + rb"file\b",
    re.IGNORECASE)
# Third-party code whose terms ask binary distributions for a notice, though no
# license file is named: the text the script looks for, and its reference key.
KNOWN_CODE = {b"Independent JPEG Group": "IJG"}


def source_references(package):
    """{reference: [source paths]} for one package: license files its sources
    point to, and KNOWN_CODE found in them. Also returns the lower-cased names
    of every file the package ships."""
    directory = os.path.dirname(package["manifest_path"])
    references, shipped = {}, set()
    for folder, subfolders, names in os.walk(directory):
        subfolders[:] = sorted(name for name in subfolders if name != "target")
        for name in sorted(names):
            shipped.add(name.lower())
            if not name.endswith(SOURCE_EXTENSIONS):
                continue
            path = os.path.join(folder, name)
            with open(path, "rb") as handle:
                data = handle.read()
            relative = os.path.relpath(path, directory).replace(os.sep, "/")
            found = {match.group(1).decode("ascii") for match in LICENSE_REFERENCE.finditer(data)}
            found |= {key for marker, key in KNOWN_CODE.items() if marker in data}
            for reference in found:
                references.setdefault(reference, []).append(relative)
    return references, shipped


def reference_covered(reference, shipped, expression):
    """Whether a referenced license file is in the notices without a supplement:
    the package ships it, or it is the crate's own license (LICENSE-MIT,
    LICENSE-APACHE, ...), whose text the notices carry anyway."""
    lowered = reference.lower()
    if lowered in shipped or any(name.startswith(lowered + ".") for name in shipped):
        return True
    suffix = re.sub(r"^(licen[cs]e|copying)[-_.]", "", lowered)
    return suffix != lowered and any(spdx.lower().startswith(suffix) for spdx in expression.ids)


def chromium_notice(package, paths):
    """AccessKit's code derived from Chromium's accessibility abstraction. Its
    sources point to LICENSE.chromium, which the packages don't ship; that file
    is Chromium's BSD-3-Clause license."""
    return [("Chromium's license (BSD-3-Clause)",
             "Parts of this crate are derived from Chromium's accessibility abstraction,\n"
             "(c) The Chromium Authors, under the BSD-style license the sources refer\n"
             "to as LICENSE.chromium:\n\n" + BSD_3_CLAUSE.format(holders="The Chromium Authors"))]


def ijg_notice(package, paths):
    """Code translated from the Independent JPEG Group's libjpeg: its terms,
    copied from the comment that opens each such source file, and the statement
    they ask executable-only distributions to make."""
    directory = os.path.dirname(package["manifest_path"])
    texts = []
    for relative in paths:
        header = re.match(r"\s*/\*(.*?)\*/", read_text(os.path.join(directory, relative)), re.S)
        if not header or "Independent JPEG Group" not in header.group(1):
            raise NoticeError("{} {}: {} mentions the Independent JPEG Group outside its opening comment".format(
                package["name"], package["version"], relative))
        texts.append(("Independent JPEG Group notice ({})".format(relative),
                      "This software is based in part on the work of the Independent JPEG Group.\n\n"
                      + header.group(1).strip("\n")))
    return texts


# (crate name, reference from source_references) -> function returning the
# (label, text) notices to add to that crate's entry.
SUPPLEMENTS = {
    ("accesskit", "LICENSE.chromium"): chromium_notice,
    ("accesskit_consumer", "LICENSE.chromium"): chromium_notice,
    ("accesskit_windows", "LICENSE.chromium"): chromium_notice,
    ("image", "IJG"): ijg_notice,
}


# Rust standard library -------------------------------------------------------

class HtmlText(html.parser.HTMLParser):
    """Plain text of COPYRIGHT-library.html: paragraphs and headings on their
    own lines, <pre> blocks (the license texts) kept as they are."""

    BREAKS = {"p", "div", "h1", "h2", "h3", "h4", "ul", "li", "details", "summary", "pre", "br"}

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.parts, self.pre, self.hidden = [], 0, 0

    def _break(self, blank):
        self.parts.append("\n\n" if blank else "\n")

    def handle_starttag(self, tag, attrs):
        if tag in ("head", "style", "script"):
            self.hidden += 1
        if tag in self.BREAKS:
            self._break(tag not in ("li", "br", "summary"))
        if tag == "li":
            self.parts.append("- ")
        if tag == "pre":
            self.pre += 1

    def handle_endtag(self, tag):
        if tag in ("head", "style", "script"):
            self.hidden = max(0, self.hidden - 1)
        if tag == "pre":
            self.pre = max(0, self.pre - 1)
        if tag in self.BREAKS:
            self._break(tag in ("p", "h1", "h2", "h3", "h4", "pre"))

    def handle_data(self, data):
        if self.hidden:
            return
        if self.pre:
            self.parts.append(data)
            return
        data = re.sub(r"\s+", " ", data)
        if not self.parts or self.parts[-1].endswith("\n"):
            data = data.lstrip()
        self.parts.append(data)

    def text(self):
        text = clean_text("".join(self.parts))
        return re.sub(r"\n{3,}", "\n\n", text)


def rust_std_notices():
    sysroot = run("rustc", "--print", "sysroot").strip()
    version = run("rustc", "--version").strip()
    docs = os.path.join(sysroot, "share", "doc", "rust")
    copyright_html = os.path.join(docs, "COPYRIGHT-library.html")
    if os.path.isfile(copyright_html):
        parser = HtmlText()
        with open(copyright_html, encoding="utf-8") as handle:
            parser.feed(handle.read())
        parser.close()
        return version, [("COPYRIGHT-library.html (as text)", parser.text())]
    licenses = [os.path.join(docs, "licenses", name) for name in ("MIT.txt", "Apache-2.0.txt")]
    if all(os.path.isfile(path) for path in licenses):
        return version, [("licenses/" + os.path.basename(path), read_text(path)) for path in licenses]
    raise NoticeError("the Rust standard library's notices are missing from {}".format(docs))


# Output --------------------------------------------------------------------------

class Notices:
    def __init__(self):
        self.lines = []
        self.seen = {}

    def add(self, *lines):
        self.lines.extend(lines)

    def heading(self, title):
        self.add("", "", RULE, title, RULE)

    def text(self, label, text, owner):
        """Writes a license text once; later copies refer to the first."""
        text = clean_text(text)
        if not text:
            return
        first = self.seen.get(text)
        if first:
            self.add("", "--- {} ---".format(label), "Same text as {}, above.".format(first))
            return
        self.seen[text] = "{} in {}".format(label, owner)
        self.add("", "--- {} ---".format(label), "", text)


def find_package(packages, name):
    for package in packages:
        if package["name"] == name:
            return package
    raise NoticeError("{} isn't in the dependency graph".format(name))


def build(target):
    policy = LicensePolicy()
    packages, graph = dependency_packages(target)
    local =[p for p in packages if p.get("source") is None]
    crates = sorted((p for p in packages if p.get("source") is not None),
                    key=lambda p: (p["name"], [int(n) if n.isdigit() else n for n in re.split(r"[.+-]", p["version"])]))

    problems = []
    for package in local:
        if package.get("license") != "MIT":
            problems.append("{} {} (in this repository) isn't MIT-licensed".format(package["name"], package["version"]))
    expressions, supplements = {}, {}
    for package in crates:
        if not package.get("license"):
            problems.append("{} {} declares no license expression".format(package["name"], package["version"]))
            continue
        expression = Expression(package["license"])
        if not expression.covered_by(policy.for_package(package)):
            problems.append("{} {}: '{}' isn't covered by windows/deny.toml".format(
                package["name"], package["version"], package["license"]))
        expressions[package["id"]] = expression
        references, shipped = source_references(package)
        for reference, paths in sorted(references.items()):
            supplement = SUPPLEMENTS.get((package["name"], reference))
            if supplement:
                supplements.setdefault(package["id"], []).append((supplement, paths))
            elif reference in KNOWN_CODE.values():
                problems.append("{} {}: {} contains {} code, whose notice SUPPLEMENTS doesn't add "
                                "for this crate".format(package["name"], package["version"], paths[0], reference))
            elif not reference_covered(reference, shipped, expression):
                problems.append("{} {}: {} refers to '{}', which the package doesn't ship; "
                                "add its notice to SUPPLEMENTS".format(
                                    package["name"], package["version"], paths[0], reference))
    if problems:
        raise NoticeError("\n  ".join(["license problems:"] + problems))

    slint = find_package(crates, "slint")
    slint_license = os.path.join(os.path.dirname(slint["manifest_path"]), "LICENSES", SLINT_ELECTED + ".md")
    # The logo's license text ships with Slint's compiler (a build-time crate).
    compiler = find_package(graph, "i-slint-compiler")
    logo_license = os.path.join(os.path.dirname(compiler["manifest_path"]), "LICENSES", "CC-BY-ND-4.0.txt")
    for path in (slint_license, logo_license):
        if not os.path.isfile(path):
            raise NoticeError("{} is missing".format(path))
    rust_version, rust_texts = rust_std_notices()
    # The interface embeds Poppins when a .slint file imports the font file.
    poppins = any("brand/fonts/Poppins" in read_text(os.path.join(WINDOWS, "ui", name))
                  for name in sorted(os.listdir(os.path.join(WINDOWS, "ui"))) if name.endswith(".slint"))

    out = Notices()
    out.add(
        "AudioSync for Windows: third-party notices",
        "",
        "Target: {}".format(target),
        "Generated from windows/Cargo.lock by scripts/windows_notices.py.",
        "",
        "audiosync.exe is AudioSync's own code (MIT) compiled together with the",
        "components below. Each is listed with its license; the full license and",
        "notice texts follow. The crates are those compiled into the executable",
        "for this target; build tools and procedural macros are left out.",
        "",
        "Contents",
        "  1. AudioSync",
        "  2. The Rust standard library",
        "  3. Slint",
        "  4. Fonts and icons",
        "  5. Rust crates ({})".format(len(crates)),
    )

    out.heading("1. AudioSync")
    app = find_package(local, ROOT_PACKAGE)
    out.add("", "AudioSync's own code, MIT License: {}.".format(
        ", ".join("{} {}".format(p["name"], p["version"]) for p in sorted(local, key=lambda p: p["name"]))),
        app.get("repository") or "")
    out.text("LICENSE", read_text(os.path.join(REPO, "LICENSE")), "AudioSync")

    out.heading("2. The Rust standard library")
    out.add("", "The parts of the Rust standard library the app uses, compiled into the",
            "executable. MIT OR Apache-2.0; its copyright notices also cover the",
            "crates it is built from. Compiler: {}.".format(rust_version),
            "https://github.com/rust-lang/rust")
    for label, text in rust_texts:
        out.text(label, text, "the Rust standard library")

    out.heading("3. Slint")
    out.add("", "Slint {}, (c) SixtyFPS GmbH, the user interface toolkit. Its crates are".format(slint["version"]),
            "offered under GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR",
            "LicenseRef-Slint-Software-3.0; AudioSync uses them under the Slint",
            "Royalty-free Desktop, Mobile, and Web Applications License 2.0.",
            "",
            "Attribution: the About dialog (Help > About AudioSync, from the app's",
            "top-level menu) shows Slint's AboutSlint widget, as section 2 of the",
            "license asks.",
            "https://slint.dev")
    out.text("LICENSES/{}.md".format(SLINT_ELECTED), read_text(slint_license),
             "the Slint section")

    out.heading("4. Fonts and icons")
    if poppins:
        out.add("", "Poppins Medium, (c) 2020 The Poppins Project Authors, the wordmark",
                "typeface, compiled into the executable. SIL Open Font License 1.1.",
                "https://github.com/itfoundry/Poppins")
        out.text("brand/fonts/OFL.txt", read_text(os.path.join(REPO, "brand", "fonts", "OFL.txt")), "AudioSync")
    out.add("", "", "Fluent UI System Icons, (c) Microsoft Corporation: Slint's Fluent style",
            "compiles icons from this set into the interface. MIT License.",
            "https://github.com/microsoft/fluentui-system-icons")
    out.text("MIT License (Fluent UI System Icons)", MIT.format(holders="Microsoft Corporation"),
             "Fluent UI System Icons")
    out.add("", "", "The Made with Slint logo, (c) SixtyFPS GmbH, shown unchanged by the",
            "AboutSlint widget. Creative Commons Attribution-NoDerivatives 4.0.")
    out.text("LICENSES/CC-BY-ND-4.0.txt", read_text(logo_license), "i-slint-compiler {}".format(compiler["version"]))
    out.add("", "", "The icons inside the app's controls come from Segoe Fluent Icons, the",
            "icon font Windows 11 ships; the app uses the installed font and doesn't",
            "include it.")

    out.heading("5. Rust crates ({})".format(len(crates)))
    for package in crates:
        expression = expressions[package["id"]]
        license_line = package["license"]
        if SLINT_ELECTED in expression.ids:
            license_line += " (used under {})".format(SLINT_ELECTED)
        out.add("", THIN_RULE, "{} {}".format(package["name"], package["version"]),
                "License: {}".format(license_line),
                "Repository: {}".format(package.get("repository") or package.get("homepage") or "not given"))
        owner = "{} {}".format(package["name"], package["version"])
        files = notice_files(package, expression)
        for label, path in files:
            out.text(label, read_text(path), owner)
        if not files:
            allowed = policy.for_package(package)
            ids = [i for i in dict.fromkeys(expression.ids) if i in TEMPLATES and i in allowed]
            # Every license the crate needs (all sides of an AND) must have a text.
            if not expression.covered_by(set(ids)):
                raise NoticeError("{} has no license file and no standard text for '{}'".format(owner, package["license"]))
            out.add("", "The package has no license file; the standard text of its license",
                    "follows, with the crate's authors as copyright holders.")
            for spdx in ids:
                out.text("{} (standard text)".format(spdx), TEMPLATES[spdx].format(holders=holders(package)), owner)
        for supplement, paths in supplements.get(package["id"], []):
            for label, text in supplement(package, paths):
                out.text(label, text, owner)
    out.add("")
    return out.lines, len(crates)


def default_output(target):
    target_dir = os.environ.get("CARGO_TARGET_DIR") or "target"
    return os.path.join(WINDOWS, target_dir, target, "release", "THIRD-PARTY-NOTICES.txt")


def main():
    parser = argparse.ArgumentParser(description="Write THIRD-PARTY-NOTICES.txt for a Windows build.")
    parser.add_argument("target", help="Rust target triple, e.g. x86_64-pc-windows-msvc")
    parser.add_argument("--output", help="file to write (default: next to audiosync.exe)")
    args = parser.parse_args()
    if not re.match(r"^[a-z0-9_]+-pc-windows-[a-z0-9]+$", args.target):
        parser.error("'{}' isn't a Windows target triple".format(args.target))
    output = args.output or default_output(args.target)
    try:
        lines, count = build(args.target)
    except NoticeError as error:
        print("windows_notices.py: {}".format(error), file=sys.stderr)
        return 1
    os.makedirs(os.path.dirname(os.path.abspath(output)), exist_ok=True)
    with open(output, "w", encoding="utf-8", newline="\r\n") as handle:
        handle.write("\n".join(lines))
    print("Wrote {}: {} crates, the Rust standard library, Slint, fonts and icons".format(
        output, count))
    return 0


if __name__ == "__main__":
    sys.exit(main())
