/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use dom::document::AbstractDocument;
use dom::element::*;
use dom::htmlelement::HTMLElement;
use dom::htmlheadingelement::{Heading1, Heading2, Heading3, Heading4, Heading5, Heading6};
use dom::htmliframeelement::IFrameSize;
use dom::htmlformelement::HTMLFormElement;
use dom::node::{AbstractNode, ElementNodeTypeId, Node, ScriptView};
use dom::types::*;
use html::cssparse::{InlineProvenance, StylesheetProvenance, UrlProvenance, spawn_css_parser};
use js::jsapi::JSContext;
use style::Stylesheet;
use script_task::page_from_context;

use std::cast;
use std::cell::Cell;
use std::comm;
use std::comm::{Port, SharedChan};
use std::str;
use std::str::eq_slice;
use std::from_str::FromStr;
use hubbub::hubbub;
use servo_msg::constellation_msg::{ConstellationChan, SubpageId};
use servo_net::image_cache_task::ImageCacheTask;
use servo_net::resource_task::{Load, Payload, Done, ResourceTask, load_whole_resource};
use servo_util::tree::{TreeNodeRef, ElementLike};
use servo_util::url::make_url;
use extra::future::Future;
use extra::url::Url;
use geom::size::Size2D;

macro_rules! handle_element(
    ($cx: expr,
     $document: expr,
     $tag: expr,
     $string: expr,
     $type_id: expr,
     $ctor: ident,
     [ $(($field:ident : $field_init:expr)),* ]) => (
        handle_element_base!(htmlelement, HTMLElement,
                             $cx, $document, $tag, $string, $type_id, $ctor,
                             [$(($field:$field_init)),*]);
    )
)
macro_rules! handle_htmlelement(
    ($cx: expr,
     $document: expr,
     $tag: expr,
     $string: expr,
     $type_id: expr,
     $ctor: ident) => (
        handle_element_base!(element, Element,
                             $cx, $document, $tag, $string, $type_id, $ctor, []);
    )
)
macro_rules! handle_newable_element(
    ($document: expr,
     $localName: expr,
     $string: expr,
     $ctor: ident
     $(, $arg:expr )*) => (
        if eq_slice($localName, $string) {
            return $ctor::new(($localName).to_str(), $document $(, $arg)*);
        }
    )
)
macro_rules! handle_element_base(
    ($parent: ident,
     $parent_init: ident,
     $cx: expr,
     $document: expr,
     $tag: expr,
     $string: expr,
     $type_id: expr,
     $ctor: ident,
     [ $(($field:ident : $field_init:expr)),* ]) => (
        if eq_slice($tag, $string) {
            let _element = @$ctor {
                $parent: $parent_init::new($type_id, ($tag).to_str(), $document),
                $(
                    $field: $field_init,
                )*
            };
            return unsafe { Node::as_abstract_node($cx, _element) };
        }
    )
)


pub struct JSFile {
    data: ~str,
    url: Url
}

type JSResult = ~[JSFile];

enum CSSMessage {
    CSSTaskNewFile(StylesheetProvenance),
    CSSTaskExit   
}

enum JSMessage {
    JSTaskNewFile(Url),
    JSTaskNewInlineScript(~str, Url),
    JSTaskExit
}

/// Messages generated by the HTML parser upon discovery of additional resources
pub enum HtmlDiscoveryMessage {
    HtmlDiscoveredStyle(Stylesheet),
    HtmlDiscoveredIFrame((Url, SubpageId, Future<Size2D<uint>>, bool)),
    HtmlDiscoveredScript(JSResult)
}

pub struct HtmlParserResult {
    root: AbstractNode<ScriptView>,
    discovery_port: Port<HtmlDiscoveryMessage>,
}

trait NodeWrapping {
    unsafe fn to_hubbub_node(self) -> hubbub::NodeDataPtr;
    unsafe fn from_hubbub_node(n: hubbub::NodeDataPtr) -> Self;
}

impl NodeWrapping for AbstractNode<ScriptView> {
    unsafe fn to_hubbub_node(self) -> hubbub::NodeDataPtr {
        cast::transmute(self)
    }
    unsafe fn from_hubbub_node(n: hubbub::NodeDataPtr) -> AbstractNode<ScriptView> {
        cast::transmute(n)
    }
}

/**
Runs a task that coordinates parsing links to css stylesheets.

This function should be spawned in a separate task and spins waiting
for the html builder to find links to css stylesheets and sends off
tasks to parse each link.  When the html process finishes, it notifies
the listener, who then collects the css rules from each task it
spawned, collates them, and sends them to the given result channel.

# Arguments

* `to_parent` - A channel on which to send back the full set of rules.
* `from_parent` - A port on which to receive new links.

*/
fn css_link_listener(to_parent: SharedChan<HtmlDiscoveryMessage>,
                     from_parent: Port<CSSMessage>,
                     resource_task: ResourceTask) {
    let mut result_vec = ~[];

    loop {
        match from_parent.recv() {
            CSSTaskNewFile(provenance) => {
                result_vec.push(spawn_css_parser(provenance, resource_task.clone()));
            }
            CSSTaskExit => {
                break;
            }
        }
    }

    // Send the sheets back in order
    // FIXME: Shouldn't wait until after we've recieved CSSTaskExit to start sending these
    for port in result_vec.iter() {
        to_parent.send(HtmlDiscoveredStyle(port.recv()));
    }
}

fn js_script_listener(to_parent: SharedChan<HtmlDiscoveryMessage>,
                      from_parent: Port<JSMessage>,
                      resource_task: ResourceTask) {
    let mut result_vec = ~[];

    loop {
        match from_parent.recv() {
            JSTaskNewFile(url) => {
                match load_whole_resource(&resource_task, url.clone()) {
                    Err(_) => {
                        error!("error loading script {:s}", url.to_str());
                    }
                    Ok((metadata, bytes)) => {
                        result_vec.push(JSFile {
                            data: str::from_utf8(bytes),
                            url: metadata.final_url,
                        });
                    }
                }
            }
            JSTaskNewInlineScript(data, url) => {
                result_vec.push(JSFile { data: data, url: url });
            }
            JSTaskExit => {
                break;
            }
        }
    }

    to_parent.send(HtmlDiscoveredScript(result_vec));
}

// Silly macros to handle constructing      DOM nodes. This produces bad code and should be optimized
// via atomization (issue #85).

pub fn build_element_from_tag(cx: *JSContext, tag: &str, document: AbstractDocument) -> AbstractNode<ScriptView> {
    // TODO (Issue #85): use atoms
    handle_element!(cx, document, tag, "a",       HTMLAnchorElementTypeId, HTMLAnchorElement, []);
    handle_element!(cx, document, tag, "applet",  HTMLAppletElementTypeId, HTMLAppletElement, []);
    handle_element!(cx, document, tag, "area",    HTMLAreaElementTypeId, HTMLAreaElement, []);
    handle_element!(cx, document, tag, "base",    HTMLBaseElementTypeId, HTMLBaseElement, []);
    handle_element!(cx, document, tag, "br",      HTMLBRElementTypeId, HTMLBRElement, []);
    handle_element!(cx, document, tag, "body",    HTMLBodyElementTypeId, HTMLBodyElement, []);
    handle_element!(cx, document, tag, "button",  HTMLButtonElementTypeId, HTMLButtonElement, []);
    handle_element!(cx, document, tag, "canvas",  HTMLCanvasElementTypeId, HTMLCanvasElement, []);
    handle_element!(cx, document, tag, "data",    HTMLDataElementTypeId, HTMLDataElement, []);
    handle_element!(cx, document, tag, "datalist",HTMLDataListElementTypeId, HTMLDataListElement, []);
    handle_element!(cx, document, tag, "dir",     HTMLDirectoryElementTypeId, HTMLDirectoryElement, []);
    handle_element!(cx, document, tag, "div",     HTMLDivElementTypeId, HTMLDivElement, []);
    handle_element!(cx, document, tag, "dl",      HTMLDListElementTypeId, HTMLDListElement, []);
    handle_element!(cx, document, tag, "embed",   HTMLEmbedElementTypeId, HTMLEmbedElement, []);
    handle_element!(cx, document, tag, "fieldset",HTMLFieldSetElementTypeId, HTMLFieldSetElement, []);
    handle_element!(cx, document, tag, "font",    HTMLFontElementTypeId, HTMLFontElement, []);
    handle_element!(cx, document, tag, "form",    HTMLFormElementTypeId, HTMLFormElement, []);
    handle_element!(cx, document, tag, "frame",   HTMLFrameElementTypeId, HTMLFrameElement, []);
    handle_element!(cx, document, tag, "frameset",HTMLFrameSetElementTypeId, HTMLFrameSetElement, []);
    handle_element!(cx, document, tag, "hr",      HTMLHRElementTypeId, HTMLHRElement, []);
    handle_element!(cx, document, tag, "head",    HTMLHeadElementTypeId, HTMLHeadElement, []);
    handle_element!(cx, document, tag, "html",    HTMLHtmlElementTypeId, HTMLHtmlElement, []);
    handle_element!(cx, document, tag, "input",   HTMLInputElementTypeId, HTMLInputElement, []);
    handle_element!(cx, document, tag, "label",   HTMLLabelElementTypeId, HTMLLabelElement, []);
    handle_element!(cx, document, tag, "legend",  HTMLLegendElementTypeId, HTMLLegendElement, []);
    handle_element!(cx, document, tag, "link",    HTMLLinkElementTypeId, HTMLLinkElement, []);
    handle_element!(cx, document, tag, "li",      HTMLLIElementTypeId, HTMLLIElement, []);
    handle_element!(cx, document, tag, "map",     HTMLMapElementTypeId, HTMLMapElement, []);
    handle_element!(cx, document, tag, "main",    HTMLMainElementTypeId, HTMLMainElement, []);
    handle_element!(cx, document, tag, "meta",    HTMLMetaElementTypeId, HTMLMetaElement, []);
    handle_element!(cx, document, tag, "meter",   HTMLMeterElementTypeId, HTMLMeterElement, []);

    handle_htmlelement!(cx, document, tag, "aside",   HTMLElementTypeId, HTMLElement);
    handle_htmlelement!(cx, document, tag, "b",       HTMLElementTypeId, HTMLElement);
    handle_htmlelement!(cx, document, tag, "i",       HTMLElementTypeId, HTMLElement);
    handle_htmlelement!(cx, document, tag, "section", HTMLElementTypeId, HTMLElement);
    handle_htmlelement!(cx, document, tag, "small",   HTMLElementTypeId, HTMLElement);

    handle_newable_element!(document, tag, "audio",     HTMLAudioElement);
    handle_newable_element!(document, tag, "caption",   HTMLTableCaptionElement);
    handle_newable_element!(document, tag, "col",       HTMLTableColElement);
    handle_newable_element!(document, tag, "colgroup",  HTMLTableColElement);
    handle_newable_element!(document, tag, "del",       HTMLModElement);
    handle_newable_element!(document, tag, "h1",        HTMLHeadingElement, Heading1);
    handle_newable_element!(document, tag, "h2",        HTMLHeadingElement, Heading2);
    handle_newable_element!(document, tag, "h3",        HTMLHeadingElement, Heading3);
    handle_newable_element!(document, tag, "h4",        HTMLHeadingElement, Heading4);
    handle_newable_element!(document, tag, "h5",        HTMLHeadingElement, Heading5);
    handle_newable_element!(document, tag, "h6",        HTMLHeadingElement, Heading6);
    handle_newable_element!(document, tag, "iframe",    HTMLIFrameElement);
    handle_newable_element!(document, tag, "img",       HTMLImageElement);
    handle_newable_element!(document, tag, "ins",       HTMLModElement);
    handle_newable_element!(document, tag, "object",    HTMLObjectElement);
    handle_newable_element!(document, tag, "ol",        HTMLOListElement);
    handle_newable_element!(document, tag, "optgroup",  HTMLOptGroupElement);
    handle_newable_element!(document, tag, "option",    HTMLOptionElement);
    handle_newable_element!(document, tag, "output",    HTMLOutputElement);
    handle_newable_element!(document, tag, "p",         HTMLParagraphElement);
    handle_newable_element!(document, tag, "param",     HTMLParamElement);
    handle_newable_element!(document, tag, "pre",       HTMLPreElement);
    handle_newable_element!(document, tag, "progress",  HTMLProgressElement);
    handle_newable_element!(document, tag, "q",         HTMLQuoteElement);
    handle_newable_element!(document, tag, "script",    HTMLScriptElement);
    handle_newable_element!(document, tag, "select",    HTMLSelectElement);
    handle_newable_element!(document, tag, "source",    HTMLSourceElement);
    handle_newable_element!(document, tag, "span",      HTMLSpanElement);
    handle_newable_element!(document, tag, "style",     HTMLStyleElement);
    handle_newable_element!(document, tag, "table",     HTMLTableElement);
    handle_newable_element!(document, tag, "tbody",     HTMLTableSectionElement);
    handle_newable_element!(document, tag, "td",        HTMLTableDataCellElement);
    handle_newable_element!(document, tag, "template",  HTMLTemplateElement);
    handle_newable_element!(document, tag, "textarea",  HTMLTextAreaElement);
    handle_newable_element!(document, tag, "th",        HTMLTableHeaderCellElement);
    handle_newable_element!(document, tag, "time",      HTMLTimeElement);
    handle_newable_element!(document, tag, "title",     HTMLTitleElement);
    handle_newable_element!(document, tag, "tr",        HTMLTableRowElement);
    handle_newable_element!(document, tag, "track",     HTMLTrackElement);
    handle_newable_element!(document, tag, "ul",        HTMLUListElement);
    handle_newable_element!(document, tag, "video",     HTMLVideoElement);

    return HTMLUnknownElement::new(tag.to_str(), document);
}

pub fn parse_html(cx: *JSContext,
                  document: AbstractDocument,
                  url: Url,
                  resource_task: ResourceTask,
                  image_cache_task: ImageCacheTask,
                  next_subpage_id: SubpageId,
                  constellation_chan: ConstellationChan) -> HtmlParserResult {
    debug!("Hubbub: parsing {:?}", url);
    // Spawn a CSS parser to receive links to CSS style sheets.
    let resource_task2 = resource_task.clone();

    let (discovery_port, discovery_chan) = comm::stream();
    let discovery_chan = SharedChan::new(discovery_chan);

    let stylesheet_chan = Cell::new(discovery_chan.clone());
    let (css_msg_port, css_msg_chan) = comm::stream();
    let css_msg_port = Cell::new(css_msg_port);
    do spawn {
        css_link_listener(stylesheet_chan.take(), css_msg_port.take(), resource_task2.clone());
    }

    let css_chan = SharedChan::new(css_msg_chan);

    // Spawn a JS parser to receive JavaScript.
    let resource_task2 = resource_task.clone();
    let js_result_chan = Cell::new(discovery_chan.clone());
    let (js_msg_port, js_msg_chan) = comm::stream();
    let js_msg_port = Cell::new(js_msg_port);
    do spawn {
        js_script_listener(js_result_chan.take(), js_msg_port.take(), resource_task2.clone());
    }
    let js_chan = SharedChan::new(js_msg_chan);

    // Wait for the LoadResponse so that the parser knows the final URL.
    let (input_port, input_chan) = comm::stream();
    resource_task.send(Load(url.clone(), input_chan));
    let load_response = input_port.recv();

    debug!("Fetched page; metadata is {:?}", load_response.metadata);

    let url2 = load_response.metadata.final_url.clone();
    let url3 = url2.clone();

    // Store the final URL before we start parsing, so that DOM routines
    // (e.g. HTMLImageElement::update_image) can resolve relative URLs
    // correctly.
    //
    // FIXME: is this safe? When we instead pass an &mut Page to parse_html,
    // we crash with a dynamic borrow failure.
    let page = page_from_context(cx);
    unsafe {
        (*page).url = Some((url2.clone(), true));
    }

    // Build the root node.
    let root = @HTMLHtmlElement { htmlelement: HTMLElement::new(HTMLHtmlElementTypeId, ~"html", document) };
    let root = unsafe { Node::as_abstract_node(cx, root) };
    debug!("created new node");
    let mut parser = hubbub::Parser("UTF-8", false);
    debug!("created parser");
    parser.set_document_node(unsafe { root.to_hubbub_node() });
    parser.enable_scripting(true);
    parser.enable_styling(true);

    let (css_chan2, css_chan3, js_chan2) = (css_chan.clone(), css_chan.clone(), js_chan.clone());
    let next_subpage_id = Cell::new(next_subpage_id);
    
    parser.set_tree_handler(~hubbub::TreeHandler {
        create_comment: |data: ~str| {
            debug!("create comment");
            let comment = @Comment::new(data, document);
            unsafe {
                Node::as_abstract_node(cx, comment).to_hubbub_node()
            }
        },
        create_doctype: |doctype: ~hubbub::Doctype| {
            debug!("create doctype");
            let ~hubbub::Doctype {name: name,
                                public_id: public_id,
                                system_id: system_id,
                                force_quirks: force_quirks } = doctype;
            let node = @DocumentType::new(name,
                                          public_id,
                                          system_id,
                                          force_quirks,
                                          document);
            unsafe {
                Node::as_abstract_node(cx, node).to_hubbub_node()
            }
        },
        create_element: |tag: ~hubbub::Tag| {
            debug!("create element");
            let node = build_element_from_tag(cx, tag.name, document);

            debug!("-- attach attrs");
            do node.as_mut_element |element| {
                for attr in tag.attributes.iter() {
                    element.set_attr(node, &Some(attr.name.clone()), &Some(attr.value.clone()));
                }
            }

            // Spawn additional parsing, network loads, etc. from tag and attrs
            match node.type_id() {
                // Handle CSS style sheets from <link> elements
                ElementNodeTypeId(HTMLLinkElementTypeId) => {
                    do node.with_imm_element |element| {
                        match (element.get_attr("rel"), element.get_attr("href")) {
                            (Some(rel), Some(href)) => {
                                if rel == "stylesheet" {
                                    debug!("found CSS stylesheet: {:s}", href);
                                    let url = make_url(href.to_str(), Some(url2.clone()));
                                    css_chan2.send(CSSTaskNewFile(UrlProvenance(url)));
                                }
                            }
                            _ => {}
                        }
                    }
                }

                ElementNodeTypeId(HTMLIframeElementTypeId) => {
                    let iframe_chan = Cell::new(discovery_chan.clone());
                    do node.with_mut_iframe_element |iframe_element| {
                        let iframe_chan = iframe_chan.take();
                        let sandboxed = iframe_element.is_sandboxed();
                        let elem = &mut iframe_element.htmlelement.element;
                        let src_opt = elem.get_attr("src").map(|x| x.to_str());
                        for src in src_opt.iter() {
                            let iframe_url = make_url(src.clone(), Some(url2.clone()));
                            iframe_element.frame = Some(iframe_url.clone());
                            
                            // Size future
                            let (port, chan) = comm::oneshot();
                            let size_future = Future::from_port(port);

                            // Subpage Id
                            let subpage_id = next_subpage_id.take();
                            next_subpage_id.put_back(SubpageId(*subpage_id + 1));

                            // Pipeline Id
                            let pipeline_id = {
                                let page = page_from_context(cx);
                                unsafe { (*page).id }
                            };

                            iframe_element.size = Some(IFrameSize {
                                pipeline_id: pipeline_id,
                                subpage_id: subpage_id,
                                future_chan: Some(chan),
                                constellation_chan: constellation_chan.clone(),
                            });
                            iframe_chan.send(HtmlDiscoveredIFrame((iframe_url, subpage_id,
                                                                   size_future, sandboxed)));
                        }
                    }
                }

                //FIXME: This should be taken care of by set_attr, but we don't have
                //       access to a window so HTMLImageElement::AfterSetAttr bails.
                ElementNodeTypeId(HTMLImageElementTypeId) => {
                    do node.with_mut_image_element |image_element| {
                        image_element.update_image(image_cache_task.clone(), Some(url2.clone()));
                    }
                }

                _ => {}
            }

            unsafe { node.to_hubbub_node() }
        },
        create_text: |data: ~str| {
            debug!("create text");
            let text = @Text::new(data, document);
            unsafe { Node::as_abstract_node(cx, text).to_hubbub_node() }
        },
        ref_node: |_| {},
        unref_node: |_| {},
        append_child: |parent: hubbub::NodeDataPtr, child: hubbub::NodeDataPtr| {
            unsafe {
                debug!("append child {:x} {:x}", parent, child);
                let parent: AbstractNode<ScriptView> = NodeWrapping::from_hubbub_node(parent);
                let child: AbstractNode<ScriptView> = NodeWrapping::from_hubbub_node(child);
                // FIXME this needs to be AppendChild.
                //       Probably blocked on #838, so that we can remove the
                //       double root element.
                parent.add_child(child, None);
            }
            child
        },
        insert_before: |_parent, _child| {
            debug!("insert before");
            0u
        },
        remove_child: |_parent, _child| {
            debug!("remove child");
            0u
        },
        clone_node: |_node, deep| {
            debug!("clone node");
            if deep { error!("-- deep clone unimplemented"); }
            fail!(~"clone node unimplemented")
        },
        reparent_children: |_node, _new_parent| {
            debug!("reparent children");
            0u
        },
        get_parent: |_node, _element_only| {
            debug!("get parent");
            0u
        },
        has_children: |_node| {
            debug!("has children");
            false
        },
        form_associate: |_form, _node| {
            debug!("form associate");
        },
        add_attributes: |_node, _attributes| {
            debug!("add attributes");
        },
        set_quirks_mode: |_mode| {
            debug!("set quirks mode");
        },
        encoding_change: |_encname| {
            debug!("encoding change");
        },
        complete_script: |script| {
            unsafe {
                let scriptnode: AbstractNode<ScriptView> = NodeWrapping::from_hubbub_node(script);
                do scriptnode.with_imm_element |script| {
                    match script.get_attr("src") {
                        Some(src) => {
                            debug!("found script: {:s}", src);
                            let new_url = make_url(src.to_str(), Some(url3.clone()));
                            js_chan2.send(JSTaskNewFile(new_url));
                        }
                        None => {
                            let mut data = ~[];
                            debug!("iterating over children {:?}", scriptnode.first_child());
                            for child in scriptnode.children() {
                                debug!("child = {:?}", child);
                                do child.with_imm_text() |text| {
                                    data.push(text.element.data.to_str());  // FIXME: Bad copy.
                                }
                            }

                            debug!("script data = {:?}", data);
                            js_chan2.send(JSTaskNewInlineScript(data.concat(), url3.clone()));
                        }
                    }
                }
            }
            debug!("complete script");
        },
        complete_style: |style| {
            // We've reached the end of a <style> so we can submit all the text to the parser.
            unsafe {
                let style: AbstractNode<ScriptView> = NodeWrapping::from_hubbub_node(style);
                let url = FromStr::from_str("http://example.com/"); // FIXME
                let url_cell = Cell::new(url);

                let mut data = ~[];
                debug!("iterating over children {:?}", style.first_child());
                for child in style.children() {
                    debug!("child = {:?}", child);
                    do child.with_imm_text() |text| {
                        data.push(text.element.data.to_str());  // FIXME: Bad copy.
                    }
                }

                debug!("style data = {:?}", data);
                let provenance = InlineProvenance(url_cell.take().unwrap(), data.concat());
                css_chan3.send(CSSTaskNewFile(provenance));
            }
        },
    });
    debug!("set tree handler");

    debug!("loaded page");
    loop {
        match load_response.progress_port.recv() {
            Payload(data) => {
                debug!("received data");
                parser.parse_chunk(data);
            }
            Done(Err(*)) => {
                fail!("Failed to load page URL {:s}", url.to_str());
            }
            Done(*) => {
                break;
            }
        }
    }

    css_chan.send(CSSTaskExit);
    js_chan.send(JSTaskExit);

    HtmlParserResult {
        root: root,
        discovery_port: discovery_port,
    }
}

