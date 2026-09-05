import type {ReactNode} from 'react';
import Link from '@docusaurus/Link';
import Layout from '@theme/Layout';
import Heading from '@theme/Heading';

import styles from './index.module.css';

const codecs = [
  ['Motion JPEG', 'Native decoding, encoding, inspection, and frame-preserving recoding.'],
  ['DV', 'DV25 video, embedded audio, timecode, metadata, decoding, and encoding.'],
  ['MPEG-2 Video', 'I/P/B decoding and encoding with dependency-aware smart rendering.'],
  ['H.264 / AVC', 'Native decoding and encoding, MP4/QuickTime playback, and clean-GOP remuxing.'],
  ['AAC', 'Native AAC-LC audio decoding and synchronized playback.'],
  ['MPEG Audio', 'Layer II framing, timing, transport, and pass-through.'],
];

export default function Home(): ReactNode {
  return (
    <Layout
      title="The hierarchical, lossless video and FX editor"
      description="MMRecode is a cross-platform Rust media layer and hierarchical video editor with native codecs, lossless recoding, and portable visual effects.">
      <main>
        <header className={styles.hero}>
          <div className={styles.wrap}>
            <div className={styles.heroBrand}>
              <img src="/img/mmrecode-mark.png" alt="" />
              <span>MMRecode</span>
            </div>
            <p className={styles.kicker}>
              Cross-platform media layer &amp; editor for all platforms
            </p>
            <Heading as="h1">The hierarchical, lossless video and FX editor.</Heading>
            <p className={styles.heroText}>
              Navigate compositions like a filesystem. Preserve what can be
              copied. Render only what changed.
            </p>
            <div className={styles.actions}>
              <Link className="button button--primary button--lg" to="/docs/get-started">
                Install with Cargo
              </Link>
              <Link to="/docs/concepts/editing-model">Read the documentation →</Link>
            </div>
            <code className={styles.install}>cargo install mmrecode</code>
          </div>
        </header>

        <section className={`${styles.wrap} ${styles.introduction}`}>
          <p>
            MMRecode is a cross-platform media layer written in Rust. It provides
            native codecs, containers, exact timing, playback, editing, render
            planning, and MMFX as independently usable components.
          </p>
          <p>
            The terminal-native, command-driven editor is the flagship
            application built on top: every media object can have its own local
            timeline, and exports preserve encoded media wherever an edit leaves
            it unchanged.
          </p>
          <p>
            MMFX adds portable titles, graphics, layouts, animation, transitions,
            and visual effects through a typed scene system with predictable
            rendering across platforms.
          </p>
          <p className={styles.founder}>
            By Markus Moenig, founder of MainConcept.com and former CTO of DivX.
          </p>
        </section>

        <section className={`${styles.wrap} ${styles.section}`}>
          <div className={styles.headingColumn}>
            <Heading as="h2">The editor runs in your terminal</Heading>
          </div>
          <div className={styles.bodyColumn}>
            <p className={styles.lead}>
              Run <code>mmrecode</code> and the complete editing workspace opens
              where you already work.
            </p>
            <p>
              The terminal contains the monitor, timeline, inspector, contextual
              help, and command prompt. Navigate hierarchical compositions with
              familiar commands, edit without changing tools, and preview moving
              video with synchronized audio in the same workspace.
            </p>
            <p>
              Kitty and Ghostty use the direct Kitty graphics path. MMRecode also
              selects Sixel or iTerm2 images when available, and falls back to a
              portable 24-bit Unicode renderer in other true-color terminals.
            </p>
          </div>
        </section>

        <section className={`${styles.wrap} ${styles.section}`}>
          <div className={styles.headingColumn}>
            <Heading as="h2">A media layer, not only an editor</Heading>
          </div>
          <div className={styles.bodyColumn}>
            <p className={styles.lead}>
              The editor is one application built on a reusable media foundation.
            </p>
            <p>
              Applications can use the codec, container, playback, editing,
              rendering, and MMFX crates independently. Shared packet, frame,
              stream, exact-time, and dependency interfaces keep those parts
              composable without tying them to one frontend or platform.
            </p>
          </div>
        </section>

        <section className={`${styles.wrap} ${styles.section}`}>
          <div className={styles.headingColumn}>
            <Heading as="h2">Lossless recoding</Heading>
          </div>
          <div className={styles.bodyColumn}>
            <p className={styles.lead}>
              MMRecode plans an export from the actual dependency structure of
              the codec instead of treating every edit as a full transcode.
            </p>
            <dl className={styles.behaviorList}>
              <div>
                <dt>Unchanged media</dt>
                <dd>Original encoded packets are copied and retimed.</dd>
              </div>
              <div>
                <dt>Exact cut boundaries</dt>
                <dd>Only codec dependencies damaged by the cut are regenerated.</dd>
              </div>
              <div>
                <dt>Titles and effects</dt>
                <dd>Only the affected frame range is decoded, rendered, and encoded.</dd>
              </div>
            </dl>
            <p>
              The render plan remains visible and explainable: copied,
              bridge-encoded, and fully rendered ranges are deliberate decisions,
              not hidden export behavior.
            </p>
          </div>
        </section>

        <section className={`${styles.wrap} ${styles.section}`}>
          <div className={styles.headingColumn}>
            <Heading as="h2">Hierarchical editing</Heading>
          </div>
          <div className={styles.bodyColumn}>
            <p className={styles.lead}>
              The composition is a hierarchy of media and placement links, not
              one permanently expanded stack of tracks.
            </p>
            <p>
              A clip may contain a title, a mask, an effect, or another complete
              composition. Entering <code>Film &gt; Clip0 &gt; Title</code> changes
              the visible timeline to that object’s local time. The media remains
              reusable, while each placement keeps its own timing and overrides.
            </p>
            <p>
              Commands such as <code>pwd</code>, <code>ls</code>, and <code>cd</code>
              expose this structure directly. Saved edits are typed operations,
              so they can be replayed, tested, diffed, or produced by another
              frontend without translating through shell strings.
            </p>
          </div>
        </section>

        <section className={`${styles.wrap} ${styles.section}`}>
          <div className={styles.headingColumn}>
            <Heading as="h2">MMFX</Heading>
          </div>
          <div className={styles.bodyColumn}>
            <p className={styles.lead}>
              MMFX is the scene, typography, layout, animation, transition, and
              visual-effect system built into MMRecode.
            </p>
            <p>
              A strict CSS-shaped scene language describes text, shapes, groups,
              media, and layout. It compiles to typed scene data before rendering;
              unknown properties are errors, time is exact, fonts are explicit
              resources, and the CPU reference renderer defines compositing
              behavior independently of a browser or GPU API.
            </p>
          </div>
        </section>

        {/* The editor screenshot will be inserted here when the final capture is available. */}

        <section className={`${styles.wrap} ${styles.codecSection}`}>
          <Heading as="h2">Current codecs</Heading>
          <p className={styles.codecIntro}>
            Codec and container implementations are separate, reusable Rust
            crates rather than adapters around a single multimedia framework.
          </p>
          <div className={styles.codecRows}>
            {codecs.map(([name, description]) => (
              <div key={name}>
                <Heading as="h3">{name}</Heading>
                <p>{description}</p>
              </div>
            ))}
          </div>
          <p className={styles.containerNote}>
            Container support includes MPEG transport streams, MP4 / QuickTime,
            and YUV4MPEG2.
          </p>
          <Link to="/docs/project-status">Detailed format documentation →</Link>
        </section>

        <section className={styles.license}>
          <div className={`${styles.wrap} ${styles.licenseInner}`}>
            <Heading as="h2">Apache 2.0</Heading>
            <div>
              <p>
                MMRecode is open source under the permissive Apache License 2.0,
                including its contributor patent grant. It can be used in open
                and proprietary media products.
              </p>
              <p>
                The source license does not replace any third-party patent
                licenses that may apply to standardized media formats.
              </p>
              <a href="https://github.com/markusmoenig/MMRecode/blob/feature/jpeg-inspect/LICENSE">
                Read the license →
              </a>
            </div>
          </div>
        </section>
      </main>
    </Layout>
  );
}
