import CodeBlock from '@theme/CodeBlock';
import React, {useEffect, useState} from "react";

export default function HephConfig() {
    const [version, setVersion] = useState('<VERSION>');

    async function fetchVersion() {
        const res = await fetch('https://storage.googleapis.com/heph-build/latest_version');

        const version = await res.text();

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
