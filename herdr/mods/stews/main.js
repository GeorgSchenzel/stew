// Stews as an inbox: the open ones by recency, then the recently closed. A running stew's
// agents sit under it; a stopped, removed or closed one has a play button (`stew start`,
// `stew open`). The list comes from `stew ls --json`; agents and their status from Herdr.

import { View, div, image, svg } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { agents, terminals, spaces, run, exec, theme, notify, openOverlay, closeOverlay, props, setAgentOrder, focused, shared, setShared } from "herdr";

// How often the list is asked for, and how long a closed stew stays in view.
const POLL = 5000;
const RECENT = 14 * 86400;
// stew's actions build boxes and clone: give them herdr's longest wait.
const LONG = 600000;

const RANK = { blocked: 4, done: 3, working: 2, idle: 1 };
const short = (id) => id.slice(id.lastIndexOf("-") + 1);
const top = (sts) => sts.reduce((m, s) => ((RANK[s] ?? 0) > (RANK[m] ?? 0) ? s : m), null);

function age(secs) {
  if (secs < 60) return "now";
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h`;
  return `${Math.floor(secs / 86400)}d`;
}

// From the agents mod: a sub-agent without a pane isn't in agents(); error counts as
// blocked, and it's never done (its parent's done is the news).
const kidStatus = (k, listed) => {
  const s = listed.get(k.address) ?? k.status;
  return s === "error" ? "blocked" : s === "done" ? "idle" : s;
};

// The agent's icon from the app's program table: a one-color glyph or a picture.
function icon(p, size, color) {
  if (!p?.icon) return null;
  return p.tint ? svg(p.icon).size(size).flex_none().text_color(color) : image(p.icon).w(size).h(size).flex_none();
}

// Its tab's name when someone named it, else the agent's own title, else the program.
function agentName(a) {
  if (a.tab_name && !/^\d+$/.test(a.tab_name)) return a.tab_name;
  const live = (a.title ?? "").replace(/^[^\p{L}\p{N}]+/u, "").trim();
  return live || a.program?.name || a.address;
}

// The last lines of what stew said, its progress lines left out.
function said(r) {
  const lines = (r.stderr || r.stdout || "").trim().split("\n").filter((l) => !/^stew: [a-z0-9-]+: /.test(l));
  return lines.slice(-8).join("\n") || `exit ${r.code}`;
}

export default class Stews extends View {
  init(_props, cx) {
    if (props()?.menu) return;
    this.stews = [];
    this.creations = [];
    this.problem = null;
    this.busy = {}; // id -> what stew is doing to it
    this.folded = new Set(); // agents with their sub-agents folded away
    this.all = localStorage.getItem("all") === "1";
    // what the row menu asks for comes through shared state; one from before this copy
    // started (kept in shared.json) is never run
    this.asked = shared("stews.ask")?.at ?? 0;
    this.refresh(cx);
    cx.timer.every(POLL, (cx) => this.refresh(cx));
  }

  refresh(cx) {
    if (this.asking) return;
    this.asking = true;
    cx.spawn(async (cx) => {
      try {
        const r = await exec("stew", ["ls", "--closed", "--json"], { timeout: 20000 });
        if (r.code === 0) {
          const v = JSON.parse(r.stdout);
          this.stews = v.stews ?? [];
          this.creations = v.creations ?? [];
          this.problem = null;
        } else this.problem = said(r);
      } catch (e) {
        this.problem = String(e);
      }
      this.asking = false;
      cx.notify();
    });
  }

  // `stew <verb> <id>`, the row busy meanwhile; what goes wrong becomes a toast.
  act(cx, verb, s, doing) {
    if (this.busy[s.id]) return;
    this.busy[s.id] = doing;
    cx.notify();
    cx.spawn(async (cx) => {
      try {
        const r = await exec("stew", [verb, s.id], { timeout: LONG });
        if (r.code !== 0) {
          notify({ title: `stew ${verb} ${short(s.id)}: ${r.code === 2 ? "refused" : "failed"}`, body: said(r), level: r.code === 2 ? "warning" : "error", timeout: 15000 });
        }
      } catch (e) {
        notify({ title: `stew ${verb} ${short(s.id)}`, body: String(e), level: "error" });
      }
      delete this.busy[s.id];
      cx.notify();
      this.refresh(cx);
    });
  }

  toggle(set, key, cx, store) {
    set.has(key) ? set.delete(key) : set.add(key);
    if (store) localStorage.setItem(store, JSON.stringify([...set]));
    cx.notify();
  }

  // The row's menu, as a popup of this mod: what fits its state. It hands what was picked
  // to the panel (the popup's own tasks end when it closes).
  menuFor(s) {
    const items = []; // [verb, label, doing, ask again first]
    if (s.state === "running") items.push(["stop", "Stop", "stopping"]);
    if (s.site && s.state !== "running") items.push(["start", "Start", "starting"]);
    if (!s.site) items.push(["open", s.state === "closed" ? "Reopen" : "Open", "opening"]);
    if (s.site) items.push(["remove", "Remove from its site", "removing", "Click again to remove (pushed work only)"]);
    if (s.state !== "closed") items.push(s.site
      ? ["close", "Close", "closing", "Click again to remove and close"]
      : ["close", "Close", "closing"]);
    openOverlay({ mod: "stews", kind: "popup", width: 260, height: 34 + items.length * 31, props: { menu: { stew: s, items } } });
  }

  menu(t) {
    const { stew, items } = props().menu;
    const ask = (verb, doing) => {
      closeOverlay(props().overlay);
      setShared("stews.ask", { verb, id: stew.id, title: stew.title, doing, at: Date.now() });
    };
    return v_flex().w_full().p(5).bg(t.popover).text_size(12.8)
      .child(div().px(8).pt(4).pb(6).text_size(11).text_color(t.dim).truncate().child(stew.title || stew.id))
      .children(items.map(([verb, label, doing, again]) => {
        // what deletes a box or dir (only once everything is pushed) takes a second click
        const armed = this.armed === verb;
        return h_flex().id(`menu-${verb}`).h(31).px(8).rounded(7).items_center()
          .text_color(armed ? t.blocked : t.foreground).hover((el) => el.bg(t.hover))
          .on_click((_e, cx) => {
            if (!again || armed) ask(verb, doing);
            else { this.armed = verb; cx.notify(); }
          })
          .child(armed ? again : label);
      }));
  }

  // A request from the row menu, once.
  asks(cx) {
    const a = shared("stews.ask");
    if (!a || a.at <= this.asked) return;
    this.asked = a.at;
    if (Date.now() - a.at > 10000) return;
    cx.spawn(async (cx) => this.act(cx, a.verb, { id: a.id, title: a.title }, a.doing));
  }

  // Live sub-agents of `addr`, depth first, each with the tree lines in front of it.
  tree(all, addr, lead = []) {
    const kids = all.filter((k) => k.parent === addr && k.state === "live");
    return kids.flatMap((k, i) => {
      const last = i === kids.length - 1;
      return [{ t: k, lines: [...lead, last ? "└" : "├"] }, ...this.tree(all, k.address, [...lead, last ? "" : "│"])];
    });
  }

  // From the agents mod: a spinner while working, a ring when idle, a dot when done.
  status(t, st, compact = false) {
    if (st === "working") return svg("mods/workspace-chrome/spinner.svg").size(12).mx(1).flex_none().text_color(t.working).spin(900);
    if (st === "blocked" && compact) return div().size(14).flex_none().rounded(4).bg(t.blocked + "26").text_size(10.5).font_bold().text_color(t.blocked).flex().items_center().justify_center().child("!");
    if (st === "blocked") return div().flex_none().px(6).h(18).rounded(5).bg(t.blocked + "26").text_size(10.5).font_semibold().text_color(t.blocked).flex().items_center().child("Needs you");
    if (st === "idle") return div().size(7).mx(3).flex_none().rounded(4).border(1.5).border_color(t.idle ?? t.dim);
    return div().size(7).mx(3).flex_none().rounded(4).bg(st === "done" ? t.done : t.dim + "00");
  }

  agentRow(t, a, subs, shown) {
    const pic = icon(a.program, 15, t.foreground);
    const open = !this.folded.has(a.address);
    return h_flex().id(`ag-${a.address}`).px(10).h(28).flex_none().items_center().gap(8).text_size(12.5)
      .when(a.focused, (el) => el.bg(t.selected))
      .cursor_pointer().hover((s) => s.bg(t.hover))
      .on_click(() => run("pane.focus", { pane: a.pane }))
      .when(Boolean(pic), (el) => el.child(pic))
      .child(div().flex_1().min_w(0).truncate().text_color(a.status === "idle" && !a.focused ? t.muted : t.foreground).child(agentName(a)))
      .when(subs.length > 0, (el) => el.child(div().id(`fold-${a.address}`).h(18).px(3).gap(3).flex().items_center().rounded(4).text_size(11)
        .text_color(open ? t.dim : (t[top(subs.map((s) => kidStatus(s.t, this.listed)))] ?? t.dim))
        .hover((s) => s.bg(t.hover).text_color(t.foreground))
        .on_click((_e, cx) => { cx.stop_propagation(); this.toggle(this.folded, a.address, cx); })
        .child(svg(`mods/workspace-chrome/chevron-${open ? "down" : "right"}.svg`).size(11).flex_none())
        .when(!open, (el) => el.child(String(subs.length)))))
      .child(this.status(t, a.status, true));
  }

  subRow(t, s, shown) {
    const k = s.t;
    const pic = icon(k.program, 13, t.foreground);
    const on = k.address === shown;
    return h_flex().id(`sub-${k.address}`).pl(18).pr(10).h(25).flex_none().items_center().text_size(12)
      .when(on, (el) => el.bg(t.raised))
      .hover((st) => st.bg(t.hover))
      .on_click(() => (this.listed.has(k.address) ? run("pane.focus", { terminal: k.address }) : run("terminal.peek", { terminal: k.address })))
      .children(s.lines.map((l) => div().self_start().mt(-4).w(9).h(17).mr(4).flex_none()
        .when(Boolean(l), (el) => el.border_l(1.5).border_color(t.input))
        .when(l === "└" || l === "├", (el) => el.border_b(1.5).rounded_bl(6))))
      .when(Boolean(pic), (el) => el.child(div().mr(7).child(pic)))
      .child(div().flex_1().truncate().text_color(on ? t.foreground : t.muted).child(k.name || k.template || k.program?.name || k.address))
      .child(this.status(t, kidStatus(k, this.listed), true));
  }

  // A small faint word.
  tag(t, text, color) {
    return div().flex_none().text_size(11).text_color(color ?? t.dim).child(text);
  }

  // Two lines: the title with its short id on the right; under it what it's doing (its
  // agents' most urgent status, or its state), where it is, and the play button.
  stewHead(t, s, mine, now) {
    const running = s.state === "running";
    const busy = this.busy[s.id];
    const closed = s.state === "closed";
    const st = top(mine.map((a) => a.status));
    // what the play button does: a stopped stew starts, one on no site is opened there again
    const play = busy || running ? null : s.state === "stopped" ? ["start", "starting"] : !s.site ? ["open", "opening"] : null;
    const words = [busy ? `${busy}…` : s.state, s.site, !running && s.updated > 0 ? age(now - s.updated) : null];
    const first = h_flex().gap(8).items_center()
      .when(running, (el) => el.child(div().size(7).flex_none().rounded(4).bg(t.ansi2)))
      .child(div().flex_1().min_w(0).truncate()
        .when(running, (el) => el.text_size(13.5).font_semibold().text_color(t.foreground))
        .when(!running, (el) => el.text_color(closed ? t.dim : t.muted))
        .child(s.title || s.id))
      .child(this.tag(t, short(s.id)));
    const second = h_flex().h(18).gap(6).items_center()
      .when(running, (el) => el.pl(15))
      .when(Boolean(busy), (el) => el.child(svg("mods/workspace-chrome/spinner.svg").size(11).flex_none().text_color(t.muted).spin(900)))
      .child(div().flex_1().min_w(0).truncate().text_size(11).text_color(running || busy ? t.muted : t.dim).child(words.filter(Boolean).join(" · ")))
      .when(running && Boolean(st), (el) => el.child(this.status(t, st, true)))
      .when(Boolean(play), (el) => el.child(div().id(`play-${s.id}`).flex_none().p(3).rounded(5)
        .text_color(t.muted).hover((el) => el.bg(t.raised).text_color(t.foreground))
        .tooltip(play?.[0] === "start" ? "Start it" : "Open it again (a new box, its repos cloned)")
        .on_click((_e, cx) => { cx.stop_propagation(); this.act(cx, play[0], s, play[1]); })
        .child(svg("mods/stews/play.svg").size(12))));
    return v_flex().gap(2).child(first).child(second);
  }

  // A running stew: a card, its agents inside under a line. Click: its space.
  card(t, s, space, mine, agentRows, now) {
    const on = Boolean(space?.active);
    return v_flex().id(`stew-${s.id}`).mb(6).flex_none().rounded(10).overflow_hidden()
      .bg(t.background).border_1().border_color(on ? t.accent : t.border)
      .on_mouse_down("right", () => this.menuFor(s))
      .child(div().id(`head-${s.id}`).px(10).py(8)
        .when(Boolean(space), (el) => el.cursor_pointer().hover((el) => el.bg(t.hover))
          .on_click(() => run("space.select", { id: space.id })))
        .child(this.stewHead(t, s, mine, now)))
      .when(agentRows.length > 0, (el) => el.child(v_flex().py(3).border_t(1).border_color(t.border).children(agentRows)));
  }

  // A stew that isn't running: a plain row.
  row(t, s, now) {
    return div().id(`stew-${s.id}`).px(10).py(6).flex_none().rounded(8)
      .hover((el) => el.bg(t.hover))
      .on_mouse_down("right", () => this.menuFor(s))
      .child(this.stewHead(t, s, [], now));
  }

  section(t, id, label, n) {
    return h_flex().id(`sec-${id}`).px(10).pt(10).pb(4).flex_none().gap(6).items_center().text_size(11).font_medium()
      .child(div().text_color(t.dim).child(label))
      .child(div().text_color(t.dim).opacity(0.7).child(String(n)));
  }

  render(cx) {
    const t = theme();
    if (props()?.menu) return this.menu(t);
    this.asks(cx);
    const now = Math.floor(Date.now() / 1000);
    const list = agents();
    this.listed = new Map(list.map((a) => [a.address, a.status]));
    const all = terminals();
    const shown = focused()?.agent;
    const byMachine = new Map(spaces().filter((s) => s.machine).map((s) => [s.machine, s]));
    const recent = (s) => s.state !== "closed" || this.all || !s.closed || now - Date.parse(s.closed) / 1000 < RECENT;
    const hidden = this.stews.filter((s) => !recent(s)).length;
    const order = [];
    const shownStews = this.stews.filter(recent);
    const running = shownStews.filter((s) => s.state === "running");
    const resting = shownStews.filter((s) => s.state !== "running" && s.state !== "closed");
    const closed = shownStews.filter((s) => s.state === "closed");
    const cards = running.map((s) => {
      const space = byMachine.get(s.id);
      const mine = space ? list.filter((a) => a.space === space.id) : [];
      // a kept sub-agent (with a pane) shows in its parent's tree, not twice
      const nested = new Set(mine.flatMap((a) => this.tree(all, a.address).map((x) => x.t.address)));
      const leads = mine.filter((a) => !nested.has(a.address));
      order.push(...leads.map((a) => a.address));
      const agentRows = leads.flatMap((a) => {
        const subs = this.tree(all, a.address);
        return [this.agentRow(t, a, subs, shown), ...(this.folded.has(a.address) ? [] : subs.map((x) => this.subRow(t, x, shown)))];
      });
      return this.card(t, s, space, mine, agentRows, now);
    });
    const group = (id, label, items) => (items.length ? [this.section(t, id, label, items.length), ...items] : []);
    const rows = [
      ...group("running", "Running", cards),
      ...group("resting", "Not running", resting.map((s) => this.row(t, s, now))),
      ...group("closed", "Closed", closed.map((s) => this.row(t, s, now))),
    ];
    setAgentOrder(order);
    const creating = this.creations.map((c) => h_flex().id(`new-${c.id}`).px(8).h(28).flex_none().items_center().gap(7).text_size(12)
      .text_color(c.state === "failed" ? t.blocked : t.muted)
      .child(div().flex_1().min_w(0).truncate().child(c.id))
      .when(c.state === "failed", (el) => el.tooltip(c.error ?? "failed").child(this.tag(t, "failed", t.blocked)))
      .when(c.state !== "failed", (el) => el.child(this.tag(t, "creating…"))
        .child(svg("mods/workspace-chrome/spinner.svg").size(12).flex_none().text_color(t.muted).spin(900))));
    const more = (hidden > 0 || this.all) ? [h_flex().id("all").px(8).h(28).flex_none().items_center().text_size(11.5).text_color(t.dim)
      .cursor_pointer().hover((el) => el.text_color(t.foreground))
      .on_click((_e, cx) => { this.all = !this.all; localStorage.setItem("all", this.all ? "1" : "0"); cx.notify(); })
      .child(this.all ? "Hide the older closed ones" : `${hidden} older closed`)] : [];
    return v_flex().size_full().min_h(0).bg(t.chrome).text_size(13)
      .child(h_flex().px(16).h(36).pt(10).flex_none().items_center().justify_between()
        .child(div().text_size(11.5).font_medium().text_color(t.dim).child("Stews"))
        .when(Boolean(this.problem), (el) => el.child(div().id("problem").text_size(11).text_color(t.blocked).tooltip(this.problem).child("stew ls failed"))))
      .when(!this.stews.length && !this.creations.length, (el) => el.child(div().px(16).text_size(12).text_color(t.dim)
        .child(this.problem ? this.problem : "No stews yet: `stew new` makes one.")))
      .child(v_flex().id("stews-list").flex_1().min_h(0).px(8).overflow_y_scrollbar().children([...creating, ...rows, ...more]));
  }
}
