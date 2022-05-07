#![allow(dead_code)]

use std::{
  borrow::{BorrowMut, Cow},
  cell::RefCell,
  ops::{Deref, DerefMut},
  rc::Rc,
};

use cssparser::{ParseError, Parser, ParserInput};
use napi::{
  bindgen_prelude::*, CallContext, JsNull, JsNumber, JsObject, JsString, JsUndefined, JsUnknown, NapiValue, Ref,
};
use napi_derive::{js_function, napi};
use parcel_css::{
  declaration::DeclarationBlock,
  error::ParserError,
  media_query::{MediaList, MediaQuery},
  properties::{Property, PropertyId},
  rules::{
    keyframes::{Keyframe, KeyframeSelector, KeyframesRule},
    style::StyleRule,
    CssRule, CssRuleList,
  },
  stylesheet::{MinifyOptions, ParserOptions, PrinterOptions, StyleSheet},
  traits::{Parse, ToCss},
};

// https://drafts.csswg.org/cssom/#the-cssstylesheet-interface
#[napi(js_name = "CSSStyleSheet")]
struct CSSStyleSheet {
  stylesheet: StyleSheet<'static>,
  rules: Option<Reference<CSSRuleList>>,
}

#[napi]
impl CSSStyleSheet {
  #[napi(constructor)]
  pub fn new(env: Env) -> Self {
    CSSGroupingRule::init(env);
    CSSStyleSheet {
      stylesheet: StyleSheet::new(
        vec!["empty.css".into()],
        CssRuleList(Vec::new()),
        ParserOptions::default(),
      ),
      rules: None,
    }
  }

  #[napi]
  pub fn replace_sync(&mut self, env: Env, code: String) -> Result<()> {
    // Disconnect all existing rules from the stylesheet.
    if let Some(rules) = &mut self.rules {
      let rules = &mut **rules;
      for (index, rule) in rules.rules.iter_mut().enumerate() {
        if let Some(rule) = rule {
          let rule: &mut CSSRule = get_reference(env, rule)?;
          rule.inner = RuleInner::Disconnected(RuleOrKeyframe::Rule(self.stylesheet.rules.0[index].clone()));
        }
      }
    }

    self.stylesheet = StyleSheet::parse("empty.css", leak_str(code), ParserOptions::default()).unwrap();
    Ok(())
  }

  #[napi(getter)]
  pub fn css_rules(&mut self, env: Env, reference: Reference<CSSStyleSheet>) -> Result<Reference<CSSRuleList>> {
    if let Some(rules) = &self.rules {
      return rules.clone(env);
    }

    let rules = CSSRuleList {
      rule_list: RuleListReference::StyleSheet(
        reference
          .clone(env)?
          .share_with(env, |stylesheet| Ok(&mut stylesheet.stylesheet.rules.0))?,
      ),
      rules: Vec::new(),
      parent_rule: None,
      stylesheet_reference: reference,
    };

    self.rules = Some(CSSRuleList::into_reference(rules, env)?);
    self.rules.as_ref().unwrap().clone(env)
  }

  #[napi]
  pub fn insert_rule(&mut self, env: Env, rule: String, index: Option<u32>) -> Result<u32> {
    insert_rule(&mut self.stylesheet.rules.0, &mut self.rules, env, rule, index)
  }

  #[napi]
  pub fn add_rule(&mut self, env: Env, selector: String, style: String, index: Option<u32>) -> Result<i32> {
    // https://drafts.csswg.org/cssom/#dom-cssstylesheet-addrule

    let string = format!("{} {{ {} }}", selector, style);
    self.insert_rule(env, string, index)?;

    Ok(-1)
  }

  #[napi]
  pub fn delete_rule(&mut self, env: Env, index: u32) -> Result<()> {
    delete_rule(&mut self.stylesheet.rules.0, &mut self.rules, env, index as usize)
  }

  #[napi]
  pub fn remove_rule(&mut self, env: Env, index: u32) -> Result<()> {
    self.delete_rule(env, index)
  }
}

fn insert_rule(
  rules: &mut Vec<CssRule<'static>>,
  js_rules: &mut Option<Reference<CSSRuleList>>,
  env: Env,
  rule: String,
  index: Option<u32>,
) -> Result<u32> {
  // https://drafts.csswg.org/cssom/#insert-a-css-rule
  let index = index.unwrap_or(0) as usize;
  if index > rules.len() {
    return Err(napi::Error::new(
      napi::Status::GenericFailure,
      "Index out of bounds".into(),
    ));
  }

  let new_rule = CssRule::parse_string(leak_str(rule), ParserOptions::default()).unwrap();

  // TODO: Check if new_rule can be inserted at position (e.g. @import)

  // Invalidate existing rule indices.
  if let Some(rules) = js_rules {
    let rules = &mut *rules;

    for rule in &rules.rules[index..] {
      if let Some(rule) = rule {
        let rule: &mut CSSRule = get_reference(env, rule)?;
        if let RuleInner::Connected { index, .. } = &mut rule.inner {
          *index += 1;
        }
      }
    }

    rules.rules.insert(index, None);
  }

  rules.insert(index, new_rule);
  Ok(index as u32)
}

fn delete_rule<T>(
  rules: &mut Vec<T>,
  js_rules: &mut Option<Reference<CSSRuleList>>,
  env: Env,
  index: usize,
) -> Result<()> {
  // https://drafts.csswg.org/cssom/#remove-a-css-rule
  if index > rules.len() {
    return Err(napi::Error::new(
      napi::Status::GenericFailure,
      "Index out of bounds".into(),
    ));
  }

  if let Some(rule_refs) = js_rules {
    rule_refs.delete_rule(env, index)?;
  }

  rules.remove(index);
  Ok(())
}

fn get_reference<T: napi::bindgen_prelude::FromNapiMutRef>(
  env: Env,
  reference: &Ref<()>,
) -> Result<&'static mut T> {
  let obj: JsUnknown = env.get_reference_value(reference)?;
  unsafe {
    let napi_value = napi::bindgen_prelude::ToNapiValue::to_napi_value(env.raw(), obj)?;
    T::from_napi_mut_ref(env.raw(), napi_value)
  }
}

enum RuleOrKeyframe {
  Rule(CssRule<'static>),
  Keyframe(Keyframe<'static>),
}

enum RuleOrKeyframeRef<'a> {
  Rule(&'a CssRule<'static>),
  Keyframe(&'a Keyframe<'static>),
}

enum RuleOrKeyframeRefMut<'a> {
  Rule(&'a mut CssRule<'static>),
  Keyframe(&'a mut Keyframe<'static>),
}

impl RuleOrKeyframe {
  fn js_value(&self, env: Env, css_rule: CSSRule) -> Result<JsUnknown> {
    match self {
      RuleOrKeyframe::Rule(rule) => css_rule_to_js_unknown(rule, env, css_rule),
      RuleOrKeyframe::Keyframe(keyframe) => keyframe_to_js_unknown(env, css_rule),
    }
  }
}

fn css_rule_to_js_unknown(rule: &CssRule<'static>, env: Env, css_rule: CSSRule) -> Result<JsUnknown> {
  let napi_value = match rule {
    CssRule::Style(_) => {
      let rule = CSSStyleRule::new(css_rule);
      unsafe { napi::bindgen_prelude::ToNapiValue::to_napi_value(env.raw(), rule)? }
    }
    CssRule::Media(_) => {
      let rule = CSSMediaRule {
        rule: CSSConditionRule {
          rule: CSSGroupingRule {
            rule: css_rule,
            rules: None,
          },
        },
        media: None,
      };
      unsafe { napi::bindgen_prelude::ToNapiValue::to_napi_value(env.raw(), rule)? }
    }
    CssRule::Supports(_) => {
      let rule = CSSSupportsRule {
        rule: CSSConditionRule {
          rule: CSSGroupingRule {
            rule: css_rule,
            rules: None,
          },
        },
      };
      unsafe { napi::bindgen_prelude::ToNapiValue::to_napi_value(env.raw(), rule)? }
    }
    CssRule::Keyframes(_) => {
      let rule = CSSKeyframesRule {
        rule: css_rule,
        rules: None,
      };
      unsafe { napi::bindgen_prelude::ToNapiValue::to_napi_value(env.raw(), rule)? }
    }
    _ => unreachable!(),
  };

  unsafe { napi::JsUnknown::from_napi_value(env.raw(), napi_value) }
}

fn keyframe_to_js_unknown(env: Env, css_rule: CSSRule) -> Result<JsUnknown> {
  let rule = CSSKeyframeRule { rule: css_rule };
  let napi_value = unsafe { napi::bindgen_prelude::ToNapiValue::to_napi_value(env.raw(), rule)? };
  unsafe { napi::JsUnknown::from_napi_value(env.raw(), napi_value) }
}

enum RuleListReference {
  StyleSheet(SharedReference<CSSStyleSheet, &'static mut Vec<CssRule<'static>>>),
  Rule(SharedReference<CSSGroupingRule, &'static mut Vec<CssRule<'static>>>),
  Keyframes(SharedReference<CSSKeyframesRule, &'static mut Vec<Keyframe<'static>>>),
}

impl RuleListReference {
  fn clone(&self, env: Env) -> Result<Self> {
    match self {
      RuleListReference::StyleSheet(s) => Ok(RuleListReference::StyleSheet(s.clone(env)?)),
      RuleListReference::Rule(r) => Ok(RuleListReference::Rule(r.clone(env)?)),
      RuleListReference::Keyframes(k) => Ok(RuleListReference::Keyframes(k.clone(env)?)),
    }
  }

  fn len(&self) -> usize {
    match self {
      RuleListReference::StyleSheet(s) => s.len(),
      RuleListReference::Rule(r) => r.len(),
      RuleListReference::Keyframes(k) => k.len(),
    }
  }

  fn rule(&self, index: usize) -> RuleOrKeyframeRef {
    let rule_list = match self {
      RuleListReference::StyleSheet(s) => &**s,
      RuleListReference::Rule(r) => &**r,
      RuleListReference::Keyframes(keyframes) => return RuleOrKeyframeRef::Keyframe(&keyframes[index]),
    };

    RuleOrKeyframeRef::Rule(&rule_list[index])
  }

  fn rule_mut(&mut self, index: usize) -> RuleOrKeyframeRefMut {
    let rule_list = match self {
      RuleListReference::StyleSheet(s) => &mut **s,
      RuleListReference::Rule(r) => &mut **r,
      RuleListReference::Keyframes(keyframes) => return RuleOrKeyframeRefMut::Keyframe(&mut keyframes[index]),
    };

    RuleOrKeyframeRefMut::Rule(&mut rule_list[index])
  }

  fn get(&self, env: Env, index: usize, css_rule: CSSRule) -> Result<JsUnknown> {
    let rule_list = match self {
      RuleListReference::StyleSheet(s) => &**s,
      RuleListReference::Rule(r) => &**r,
      RuleListReference::Keyframes(keyframes) => match keyframes.get(index) {
        Some(_) => return keyframe_to_js_unknown(env, css_rule),
        None => return Ok(env.get_null()?.into_unknown()),
      },
    };

    let rule = match rule_list.get(index) {
      Some(rule) => rule,
      None => return Ok(env.get_null()?.into_unknown()),
    };

    css_rule_to_js_unknown(rule, env, css_rule)
  }
}

#[napi(js_name = "CSSRuleList")]
struct CSSRuleList {
  rule_list: RuleListReference,
  rules: Vec<Option<Ref<()>>>,
  parent_rule: Option<Reference<CSSRule>>,
  stylesheet_reference: Reference<CSSStyleSheet>,
}

#[napi]
impl CSSRuleList {
  #[napi(constructor)]
  pub fn new() -> Self {
    unreachable!()
  }

  #[napi(getter)]
  pub fn length(&self) -> u32 {
    self.rule_list.len() as u32
  }

  #[napi]
  pub fn item(&mut self, index: u32, env: Env) -> Result<JsUnknown> {
    let index = index as usize;
    if let Some(Some(rule)) = self.rules.get(index) {
      return env.get_reference_value(rule);
    }

    let css_rule = CSSRule {
      inner: RuleInner::Connected {
        rule_list: self.rule_list.clone(env)?,
        index,
      },
      parent_rule: if let Some(parent_rule) = &self.parent_rule {
        Some(parent_rule.clone(env)?)
      } else {
        None
      },
      parent_stylesheet: self.stylesheet_reference.clone(env)?,
    };

    let js_value = self.rule_list.get(env, index, css_rule)?;

    if self.rules.len() <= index {
      self.rules.resize_with(index + 1, || None);
    }

    self.rules[index] = Some(env.create_reference_with_refcount(&js_value, 0)?);
    Ok(js_value)
  }

  fn delete_rule(&mut self, env: Env, index: usize) -> Result<()> {
    // https://drafts.csswg.org/cssom/#remove-a-css-rule
    if index < self.rules.len() {
      if let Some(rule) = &self.rules[index] {
        let rule: &mut CSSRule = get_reference(env, rule)?;
        rule.inner.disconnect();
        rule.parent_rule = None;
      }

      for rule in &self.rules[index + 1..] {
        if let Some(rule) = rule {
          let rule: &mut CSSRule = get_reference(env, rule)?;
          if let RuleInner::Connected { index, .. } = &mut rule.inner {
            *index -= 1;
          }
        }
      }

      self.rules.remove(index);
    }

    Ok(())
  }
}

enum RuleInner {
  Connected { rule_list: RuleListReference, index: usize },
  Disconnected(RuleOrKeyframe),
}

impl RuleInner {
  fn rule(&self) -> RuleOrKeyframeRef {
    match self {
      RuleInner::Connected { rule_list, index } => rule_list.rule(*index),
      RuleInner::Disconnected(rule) => match rule {
        RuleOrKeyframe::Rule(rule) => RuleOrKeyframeRef::Rule(rule),
        RuleOrKeyframe::Keyframe(keyframe) => RuleOrKeyframeRef::Keyframe(keyframe),
      },
    }
  }

  fn rule_mut(&mut self) -> RuleOrKeyframeRefMut {
    match self {
      RuleInner::Connected { rule_list, index } => rule_list.rule_mut(*index),
      RuleInner::Disconnected(rule) => match rule {
        RuleOrKeyframe::Rule(rule) => RuleOrKeyframeRefMut::Rule(rule),
        RuleOrKeyframe::Keyframe(keyframe) => RuleOrKeyframeRefMut::Keyframe(keyframe),
      },
    }
  }

  fn disconnect(&mut self) {
    *self = RuleInner::Disconnected(match self.rule() {
      RuleOrKeyframeRef::Rule(rule) => RuleOrKeyframe::Rule(rule.clone()),
      RuleOrKeyframeRef::Keyframe(keyframe) => RuleOrKeyframe::Keyframe(keyframe.clone()),
    })
  }
}

#[napi(js_name = "CSSRule")]
struct CSSRule {
  inner: RuleInner,
  parent_rule: Option<Reference<CSSRule>>,
  parent_stylesheet: Reference<CSSStyleSheet>,
}

#[napi]
impl CSSRule {
  #[napi(constructor)]
  pub fn new() -> Self {
    unreachable!()
  }

  #[napi(getter, js_name = "type")]
  pub fn kind(&self) -> u32 {
    match self.inner.rule() {
      RuleOrKeyframeRef::Rule(rule) => match rule {
        CssRule::Style(..) => 1,
        CssRule::Import(..) => 3,
        CssRule::Media(..) => 4,
        CssRule::FontFace(..) => 5,
        CssRule::Page(..) => 6,
        CssRule::Keyframes(..) => 7,
        CssRule::Namespace(..) => 10,
        CssRule::CounterStyle(..) => 11,
        CssRule::Supports(..) => 12,
        CssRule::Viewport(..) => 15,
        _ => 0,
      },
      RuleOrKeyframeRef::Keyframe(_) => 8,
    }
  }

  #[napi(getter)]
  pub fn css_text(&self) -> String {
    match self.inner.rule() {
      RuleOrKeyframeRef::Rule(rule) => rule.to_css_string(PrinterOptions::default()).unwrap(),
      RuleOrKeyframeRef::Keyframe(rule) => rule.to_css_string(PrinterOptions::default()).unwrap(),
    }
  }

  #[napi(setter)]
  pub fn set_css_text(&self) {
    // On setting the cssText attribute must do nothing.
  }

  fn rule(&self) -> &CssRule<'static> {
    match self.inner.rule() {
      RuleOrKeyframeRef::Rule(rule) => rule,
      _ => unreachable!(),
    }
  }

  fn rule_mut(&mut self) -> &mut CssRule<'static> {
    match self.inner.rule_mut() {
      RuleOrKeyframeRefMut::Rule(rule) => rule,
      _ => unreachable!(),
    }
  }

  #[napi(getter)]
  pub fn parent_style_sheet(&self, env: Env) -> Result<JsUnknown> {
    match &self.inner {
      RuleInner::Connected { .. } => unsafe {
        let value =
          napi::bindgen_prelude::ToNapiValue::to_napi_value(env.raw(), self.parent_stylesheet.clone(env))?;
        napi::JsUnknown::from_napi_value(env.raw(), value)
      },
      RuleInner::Disconnected(..) => Ok(env.get_null()?.into_unknown()),
    }
  }

  #[napi(getter)]
  pub fn parent_rule(&self, env: Env) -> Result<Option<Reference<CSSRule>>> {
    if let Some(parent) = &self.parent_rule {
      return Ok(Some(parent.clone(env)?));
    }

    Ok(None)
  }
}

#[napi(js_name = "CSSStyleRule")]
struct CSSStyleRule {
  rule: CSSRule,
  style: Option<Reference<CSSStyleDeclaration>>,
}

#[napi]
impl CSSStyleRule {
  fn new(rule: CSSRule) -> CSSStyleRule {
    CSSStyleRule { rule, style: None }
  }

  #[napi(constructor)]
  pub fn constructor() {
    unreachable!();
  }

  #[napi(getter)]
  pub fn selector_text(&self) -> String {
    match &*self.rule.rule() {
      CssRule::Style(style) => cssparser::ToCss::to_css_string(&style.selectors),
      _ => unreachable!(),
    }
  }

  #[napi(setter)]
  pub fn set_selector_text(&mut self, text: String) {
    match &mut *self.rule.rule_mut() {
      CssRule::Style(style) => {
        style.set_selector_text(leak_str(text)).unwrap();
      }
      _ => unreachable!(),
    }
  }

  #[napi(getter)]
  pub fn style(&mut self, env: Env, reference: Reference<CSSStyleRule>) -> Result<Reference<CSSStyleDeclaration>> {
    if let Some(rules) = &self.style {
      return rules.clone(env);
    }

    let style = CSSStyleDeclaration::into_reference(CSSStyleDeclaration { rule: reference }, env)?;
    self.style = Some(style.clone(env)?);
    Ok(style)
  }

  #[napi(setter)]
  pub fn set_style(&mut self, text: String) {
    match &mut *self.rule.rule_mut() {
      CssRule::Style(style) => {
        style.declarations = DeclarationBlock::parse_string(leak_str(text), ParserOptions::default()).unwrap();
      }
      _ => unreachable!(),
    };
  }

  fn rule(&self) -> &StyleRule<'static> {
    let rule = self.rule.rule();
    match rule {
      CssRule::Style(style) => style,
      _ => unreachable!(),
    }
  }

  fn rule_mut(&mut self) -> &mut StyleRule<'static> {
    let rule = self.rule.rule_mut();
    match rule {
      CssRule::Style(style) => style,
      _ => unreachable!(),
    }
  }
}

#[napi(js_name = "CSSStyleDeclaration")]
struct CSSStyleDeclaration {
  rule: Reference<CSSStyleRule>,
}

#[napi]
impl CSSStyleDeclaration {
  #[napi(constructor)]
  pub fn new() -> Self {
    unreachable!()
  }

  #[napi(getter)]
  pub fn parent_rule(&self, env: Env) -> Result<Reference<CSSStyleRule>> {
    self.rule.clone(env)
  }

  #[napi(getter)]
  pub fn css_text(&self) -> String {
    self.rule.rule().declarations.to_css_string(PrinterOptions::default()).unwrap()
  }

  #[napi(setter)]
  pub fn set_css_text(&mut self, text: String) {
    self.rule.set_style(text)
  }

  fn get_longhands(&self) -> Vec<String> {
    let rule = self.rule.rule();
    let mut longhands = Vec::new();
    for (property, _important) in rule.declarations.iter() {
      let property_id = property.property_id();
      if let Some(properties) = property_id.longhands() {
        longhands.extend(properties.iter().map(|property_id| property_id.name().to_owned()))
      } else {
        longhands.push(property_id.name().to_owned());
      }
    }

    return longhands;
  }

  #[napi(getter)]
  pub fn length(&self) -> u32 {
    return self.get_longhands().len() as u32;
  }

  #[napi]
  pub fn item(&self, index: u32) -> String {
    let mut longhands = self.get_longhands();
    let index = index as usize;
    if index < longhands.len() {
      return std::mem::take(&mut longhands[index]);
    }
    String::new()
  }

  #[napi]
  pub fn get_property_value(&self, property: String) -> String {
    let property_id = PropertyId::parse_string(&property).unwrap();
    let opts = PrinterOptions::default();

    if let Some((value, _important)) = self.rule.rule().declarations.get(&property_id) {
      return value.value_to_css_string(opts).unwrap();
    }

    String::new()
  }

  #[napi]
  pub fn get_property_priority(&mut self, property: String) -> &str {
    let property_id = PropertyId::parse_string(&property).unwrap();
    let important = if let Some((_value, important)) = self.rule.rule().declarations.get(&property_id) {
      important
    } else {
      false
    };

    if important {
      "important"
    } else {
      ""
    }
  }

  #[napi]
  pub fn set_property(&mut self, property: String, value: String, priority: Option<String>) {
    if value.is_empty() {
      self.remove_property(property);
      return;
    }

    let property =
      Property::parse_string(leak_str(property).into(), leak_str(value), ParserOptions::default()).unwrap();
    self.rule.rule_mut().declarations.set(
      property,
      if let Some(priority) = priority {
        priority.eq_ignore_ascii_case("important")
      } else {
        false
      },
    );
  }

  #[napi]
  pub fn remove_property(&mut self, property: String) -> String {
    let value = self.get_property_value(property.clone());

    let property_id = PropertyId::parse_string(&property).unwrap();
    self.rule.rule_mut().declarations.remove(&property_id);

    value
  }
}

// https://drafts.csswg.org/cssom-1/#the-cssgroupingrule-interface
#[napi(js_name = "CSSGroupingRule")]
struct CSSGroupingRule {
  rule: CSSRule,
  rules: Option<Reference<CSSRuleList>>,
}

#[napi]
impl CSSGroupingRule {
  #[napi(constructor)]
  pub fn new() {
    unreachable!()
  }

  fn init(env: Env) {
    let constructor_value = napi::bindgen_prelude::get_class_constructor("CSSGroupingRule\0").unwrap();
    let mut value = std::ptr::null_mut();
    unsafe { napi::sys::napi_get_reference_value(env.raw(), constructor_value, &mut value) };
    let constructor = unsafe { JsFunction::from_raw(env.raw(), value).unwrap() };
    let constructor = constructor.coerce_to_object().unwrap();
    let mut prototype: JsObject = constructor.get_named_property("prototype").unwrap();
    prototype
      .set_named_property(
        "insertRule",
        env.create_function("insertRule", grouping_rule_insert).unwrap(),
      )
      .unwrap();
    prototype
      .set_named_property(
        "deleteRule",
        env.create_function("deleteRule", grouping_rule_delete).unwrap(),
      )
      .unwrap();
  }

  #[napi(getter)]
  pub fn css_rules(&mut self, env: Env, reference: Reference<CSSGroupingRule>) -> Result<Reference<CSSRuleList>> {
    if let Some(rules) = &self.rules {
      return rules.clone(env);
    }

    let rules = CSSRuleList {
      rule_list: RuleListReference::Rule(reference.clone(env)?.share_with(
        env,
        |rule| match rule.rule.rule_mut() {
          CssRule::Media(media) => Ok(&mut media.rules.0),
          CssRule::Supports(supports) => Ok(&mut supports.rules.0),
          _ => unreachable!(),
        },
      )?),
      rules: Vec::new(),
      parent_rule: Some(unsafe {
        std::mem::transmute::<Reference<CSSGroupingRule>, Reference<CSSRule>>(reference)
      }),
      stylesheet_reference: self.rule.parent_stylesheet.clone(env)?,
    };

    self.rules = Some(CSSRuleList::into_reference(rules, env)?);
    self.rules.as_ref().unwrap().clone(env)
  }

  // #[napi]
  // pub fn insert_rule(&mut self, env: Env, rule: String, index: Option<u32>) -> Result<u32> {
  //   let rules = match self.rule.rule_mut() {
  //     CssRule::Media(media) => &mut media.rules.0,
  //     _ => unreachable!(),
  //   };
  //   insert_rule(rules, &mut self.rules, env, rule, index)
  // }

  // #[napi]
  // pub fn delete_rule(&mut self, env: Env, index: u32) -> Result<()> {
  //   let rules = match self.rule.rule_mut() {
  //     CssRule::Media(media) => &mut media.rules.0,
  //     _ => unreachable!(),
  //   };
  //   delete_rule(rules, &mut self.rules, env, index)
  // }
}

// Inheritance doesn't work with methods. v8 throws "Illegal invocation" errors due to signature checks.
// https://github.com/nodejs/node-addon-api/issues/246
// Instead, define a pure JS function here and assign it to the prototype of the class manually.
#[js_function(2)]
fn grouping_rule_insert(ctx: CallContext) -> Result<JsNumber> {
  let this: JsObject = ctx.this()?;
  // This is probably extremely unsafe.
  // TODO: use napi_type_tag_object?
  let napi_value = unsafe { napi::bindgen_prelude::ToNapiValue::to_napi_value(ctx.env.raw(), this).unwrap() };
  let rule = unsafe { CSSGroupingRule::from_napi_mut_ref(ctx.env.raw(), napi_value).unwrap() };
  let rules = match rule.rule.rule_mut() {
    CssRule::Media(media) => &mut media.rules.0,
    CssRule::Supports(supports) => &mut supports.rules.0,
    _ => unreachable!(),
  };
  let new_rule: JsString = ctx.get(0)?;
  let utf8 = new_rule.into_utf8()?;
  let new_rule = utf8.into_owned()?;
  let index: Option<u32> = if ctx.length > 1 {
    Some(ctx.get::<JsNumber>(1)?.get_uint32()?)
  } else {
    None
  };
  let res = insert_rule(rules, &mut rule.rules, *ctx.env, new_rule, index);
  if let Ok(res) = res {
    ctx.env.create_uint32(res)
  } else {
    Err(res.unwrap_err())
  }
}

#[js_function(1)]
fn grouping_rule_delete(ctx: CallContext) -> Result<JsUndefined> {
  let this: JsObject = ctx.this()?;
  // This is probably extremely unsafe.
  // TODO: use napi_type_tag_object?
  let napi_value = unsafe { napi::bindgen_prelude::ToNapiValue::to_napi_value(ctx.env.raw(), this).unwrap() };
  let rule = unsafe { CSSGroupingRule::from_napi_mut_ref(ctx.env.raw(), napi_value).unwrap() };
  let rules = match rule.rule.rule_mut() {
    CssRule::Media(media) => &mut media.rules.0,
    CssRule::Supports(supports) => &mut supports.rules.0,
    _ => unreachable!(),
  };
  let index = ctx.get::<JsNumber>(0)?.get_uint32()?;
  delete_rule(rules, &mut rule.rules, *ctx.env, index as usize)?;
  ctx.env.get_undefined()
}

// https://drafts.csswg.org/css-conditional-3/#cssconditionrule
#[napi(js_name = "CSSConditionRule")]
struct CSSConditionRule {
  rule: CSSGroupingRule,
}

#[napi]
impl CSSConditionRule {
  #[napi(constructor)]
  pub fn new() {
    unreachable!()
  }

  #[napi(getter)]
  pub fn condition_text(&self) -> Result<String> {
    match self.rule.rule.rule() {
      CssRule::Media(media) => Ok(media.query.to_css_string(PrinterOptions::default()).unwrap()),
      CssRule::Supports(supports) => Ok(supports.condition.to_css_string(PrinterOptions::default()).unwrap()),
      _ => Err(napi::Error::new(
        napi::Status::InvalidArg,
        "Not a conditional rule".into(),
      )),
    }
  }

  #[napi(setter)]
  pub fn set_condition_text(&mut self, text: String) {
    match self.rule.rule.rule_mut() {
      CssRule::Media(media) => {
        if let Ok(media_list) = MediaList::parse_string(leak_str(text)) {
          media.query = media_list;
        }
      }
      CssRule::Supports(_) => {
        // Spec doesn't say this can be set. WebKit does nothing, Firefox throws. We do nothing.
      }
      _ => {}
    }
  }
}

// https://drafts.csswg.org/css-conditional-3/#cssmediarule
#[napi(js_name = "CSSMediaRule")]
struct CSSMediaRule {
  rule: CSSConditionRule,
  media: Option<Reference<JSMediaList>>,
}

#[napi]
impl CSSMediaRule {
  #[napi(constructor)]
  pub fn new() {
    unreachable!()
  }

  #[napi(getter)]
  pub fn media(&mut self, env: Env, reference: Reference<CSSMediaRule>) -> Result<Reference<JSMediaList>> {
    if let Some(media) = &self.media {
      return media.clone(env);
    }

    let media = JSMediaList::into_reference(
      JSMediaList {
        media_list: reference.share_with(env, |rule| match rule.rule.rule.rule.rule_mut() {
          CssRule::Media(media) => Ok(&mut media.query),
          _ => unreachable!(),
        })?,
      },
      env,
    )?;
    self.media = Some(media.clone(env)?);
    Ok(media)
  }

  #[napi(setter)]
  pub fn set_media(&mut self, text: String) {
    self.rule.set_condition_text(text)
  }
}

#[napi(js_name = "MediaList")]
struct JSMediaList {
  media_list: SharedReference<CSSMediaRule, &'static mut MediaList<'static>>,
}

#[napi]
impl JSMediaList {
  #[napi(constructor)]
  pub fn new() {
    unreachable!()
  }

  #[napi(getter)]
  pub fn media_text(&self) -> String {
    self.media_list.to_css_string(PrinterOptions::default()).unwrap()
  }

  #[napi(setter)]
  pub fn set_media_text(&mut self, text: String) {
    if let Ok(media_list) = MediaList::parse_string(leak_str(text)) {
      **self.media_list = media_list;
    }
  }

  #[napi(getter)]
  pub fn length(&self) -> u32 {
    self.media_list.media_queries.len() as u32
  }

  #[napi]
  pub fn item(&self, index: u32) -> Option<String> {
    if let Some(query) = self.media_list.media_queries.get(index as usize) {
      return Some(query.to_css_string(PrinterOptions::default()).unwrap());
    }

    None
  }

  #[napi]
  pub fn append_medium(&mut self, medium: String) {
    if let Ok(query) = MediaQuery::parse_string(leak_str(medium)) {
      if self.media_list.media_queries.contains(&query) {
        return;
      }

      self.media_list.media_queries.push(query);
    }
  }

  #[napi]
  pub fn delete_medium(&mut self, medium: String) -> Result<()> {
    if let Ok(query) = MediaQuery::parse_string(leak_str(medium)) {
      let queries = &mut self.media_list.media_queries;
      let len = queries.len();
      queries.retain(|q| *q != query);
      if queries.len() == len {
        return Err(napi::Error::new(napi::Status::GenericFailure, "Rule not found".into()));
      }
    }

    Ok(())
  }
}

// https://drafts.csswg.org/css-conditional-3/#the-csssupportsrule-interface
#[napi(js_name = "CSSSupportsRule")]
struct CSSSupportsRule {
  rule: CSSConditionRule,
}

#[napi]
impl CSSSupportsRule {
  #[napi(constructor)]
  pub fn new() {
    unreachable!()
  }
}

// https://drafts.csswg.org/css-animations-1/#csskeyframesrule
#[napi(js_name = "CSSKeyframesRule")]
struct CSSKeyframesRule {
  rule: CSSRule,
  rules: Option<Reference<CSSRuleList>>,
}

#[napi]
impl CSSKeyframesRule {
  #[napi(constructor)]
  pub fn new() {
    unreachable!()
  }

  fn rule(&self) -> Result<&KeyframesRule<'static>> {
    match self.rule.rule() {
      CssRule::Keyframes(rule) => Ok(rule),
      _ => {
        return Err(napi::Error::new(
          napi::Status::GenericFailure,
          "Not an @keyframes rule".into(),
        ))
      }
    }
  }

  fn rule_mut(&mut self) -> Result<&mut KeyframesRule<'static>> {
    match self.rule.rule_mut() {
      CssRule::Keyframes(rule) => Ok(rule),
      _ => {
        return Err(napi::Error::new(
          napi::Status::GenericFailure,
          "Not an @keyframes rule".into(),
        ))
      }
    }
  }

  #[napi(getter)]
  pub fn name(&self) -> Result<&str> {
    Ok(self.rule()?.name.0.as_ref())
  }

  #[napi(setter)]
  pub fn set_name(&mut self, name: String) -> Result<()> {
    let rule = self.rule_mut()?;
    rule.name.0 = name.into();
    Ok(())
  }

  #[napi(getter)]
  pub fn css_rules(&mut self, env: Env, reference: Reference<CSSKeyframesRule>) -> Result<Reference<CSSRuleList>> {
    if let Some(rules) = &self.rules {
      return rules.clone(env);
    }

    let rules = CSSRuleList {
      rule_list: RuleListReference::Keyframes(reference.clone(env)?.share_with(env, |rule| {
        match rule.rule.rule_mut() {
          CssRule::Keyframes(k) => Ok(&mut k.keyframes),
          _ => unreachable!(),
        }
      })?),
      rules: Vec::new(),
      parent_rule: Some(unsafe {
        std::mem::transmute::<Reference<CSSKeyframesRule>, Reference<CSSRule>>(reference)
      }),
      stylesheet_reference: self.rule.parent_stylesheet.clone(env)?,
    };

    self.rules = Some(CSSRuleList::into_reference(rules, env)?);
    self.rules.as_ref().unwrap().clone(env)
  }

  fn find_index(&self, select: String) -> Result<Option<usize>> {
    let parsed = match parse_keyframe_selectors(&select) {
      Ok(selector) => selector,
      Err(_) => return Ok(None),
    };

    // Find the _last_ matching rule.
    let rule = self.rule()?;
    let len = rule.keyframes.len();
    match rule.keyframes.iter().rev().position(|keyframe| keyframe.selectors == parsed) {
      Some(index) => Ok(Some(len - 1 - index)),
      _ => Ok(None),
    }
  }

  #[napi]
  pub fn find_rule(
    &mut self,
    select: String,
    env: Env,
    reference: Reference<CSSKeyframesRule>,
  ) -> Result<JsUnknown> {
    match self.find_index(select)? {
      Some(index) => self.css_rules(env, reference)?.item(index as u32, env),
      None => Ok(env.get_null()?.into_unknown()),
    }
  }

  #[napi]
  pub fn append_rule(&mut self, rule: String) -> Result<()> {
    if let Ok(keyframe) = Keyframe::parse_string(leak_str(rule)) {
      let rule = self.rule_mut()?;
      rule.keyframes.push(keyframe);
    }

    Ok(())
  }

  #[napi]
  pub fn delete_rule(&mut self, select: String, env: Env) -> Result<()> {
    match self.find_index(select)? {
      Some(index) => {
        let rule = match self.rule.rule_mut() {
          CssRule::Keyframes(rule) => rule,
          _ => unreachable!(),
        };
        delete_rule(&mut rule.keyframes, &mut self.rules, env, index)?;
      }
      None => {}
    }

    Ok(())
  }
}

// https://drafts.csswg.org/css-animations-1/#csskeyframerule
#[napi(js_name = "CSSKeyframeRule")]
struct CSSKeyframeRule {
  rule: CSSRule,
}

#[napi]
impl CSSKeyframeRule {
  #[napi(constructor)]
  pub fn new() {
    unreachable!()
  }

  #[napi(getter)]
  pub fn key_text(&self) -> String {
    match self.rule.inner.rule() {
      RuleOrKeyframeRef::Keyframe(keyframe) => {
        keyframe.selectors.to_css_string(PrinterOptions::default()).unwrap()
      }
      _ => unreachable!(),
    }
  }

  #[napi(setter)]
  pub fn set_key_text(&mut self, text: String) {
    if let Ok(selectors) = parse_keyframe_selectors(leak_str(text)) {
      match self.rule.inner.rule_mut() {
        RuleOrKeyframeRefMut::Keyframe(keyframe) => {
          keyframe.selectors = selectors;
        }
        _ => unreachable!(),
      }
    } else {
      // Spec says to throw a SyntaxError, but no browser does?
    }
  }
}

fn leak_str(string: String) -> &'static str {
  let res = unsafe {
    let slice = std::slice::from_raw_parts(string.as_ptr(), string.len());
    std::str::from_utf8_unchecked(slice)
  };
  std::mem::forget(string);
  res
}

fn parse_keyframe_selectors<'i>(
  text: &'i str,
) -> std::result::Result<Vec<KeyframeSelector>, ParseError<'i, ParserError<'i>>> {
  let mut input = ParserInput::new(text);
  let mut parser = Parser::new(&mut input);
  parser.parse_comma_separated(KeyframeSelector::parse)
}