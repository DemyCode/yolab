import { ConnectionField } from "./fields";
import {
  ArrayFieldTemplate,
  FieldTemplate,
  ObjectFieldTemplate,
} from "./templates";
import {
  CheckboxWidget,
  AppWidget,
  FolderWidget,
  PasswordWidget,
  ServiceUrlWidget,
  TextareaWidget,
  TunnelWidget,
  YolabTokenWidget,
} from "./widgets";

export const templates = {
  FieldTemplate,
  ObjectFieldTemplate,
  ArrayFieldTemplate,
};

export const fields = {
  ConnectionField,
};

export const widgets = {
  TunnelWidget,
  YolabTokenWidget,
  ServiceUrlWidget,
  FolderWidget,
  AppWidget,
  PasswordWidget,
  CheckboxWidget,
  TextareaWidget,
};
