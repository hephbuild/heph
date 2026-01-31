import CodeBlock from '@theme/CodeBlock';
import React, { useEffect, useState } from "react";

export default function HephConfig() {
    const [version, setVersion] = useState('<VERSION>');

    async function fetchVersion() {
        const res = await fetch('https://api.github.com/repos/hephbuild/heph-artifacts-v0/releases/latest');

        const body = await res.json();
        const version = body.tag_name;

        setVersion(version);
    }

    useEffect(() => {
        void fetchVersion();
    }, []);

    return (
        <CodeBlock language="yaml" title=".hephconfig">
            {`version: ${version}`}
        </CodeBlock>
    )
}
